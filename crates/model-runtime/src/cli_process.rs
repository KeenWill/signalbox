//! Provider-independent CLI process supervision and session orchestration.

use std::ops::{Deref, DerefMut};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};

use crate::{
    CancellationSignal, LossCause, Observation, ObservationFact, ObservationSink,
    ProvenUnsentEvidence, RedactingSink, TerminalEvidence, TransportFacts, UnsentCause,
};

const TRUNCATION_SUFFIX: &str = "… [truncated]";

/// Whether this target can supervise the CLI's complete Unix process group.
pub const CLI_PROCESS_GROUP_SUPERVISION_SUPPORTED: bool = cfg!(all(
    unix,
    not(any(
        target_os = "cygwin",
        target_os = "horizon",
        target_os = "openbsd",
        target_os = "redox",
        target_os = "wasi"
    ))
));

/// Provider names used solely to preserve adapter-specific diagnostics.
#[derive(Clone, Copy, Debug)]
pub struct CliProcessLabels {
    /// Short provider name, such as `Codex`.
    pub provider: &'static str,
    /// Process name, such as `Codex CLI`.
    pub process: &'static str,
    /// Decoder event name, such as `Codex event`.
    pub decode_event: &'static str,
    /// Bounded line name, such as `Codex JSONL event`.
    pub bounded_event: &'static str,
}

/// How the shared redactor handles provider text for terminal reconstruction.
#[derive(Clone, Copy)]
pub enum CliTerminalTextCapture {
    /// The provider decoder owns terminal text independently.
    Disabled,
    /// Capture sanitized text for terminal evidence without forwarding deltas.
    TerminalOnly,
    /// Capture sanitized text and forward the same streamed deltas.
    StreamAndTerminal,
}

/// One fully constructed provider command and its shared execution policy.
pub struct CliProcessRequest<C, D> {
    /// Provider-specific command arguments and working directory.
    pub command: std::process::Command,
    /// Full request body written to stdin.
    pub prompt: Vec<u8>,
    /// Correlation copied onto `SendCommenced`.
    pub correlation: C,
    /// Provider-specific event decoder.
    pub decoder: D,
    /// Terminal text capture and forwarding policy.
    pub terminal_text_capture: CliTerminalTextCapture,
    /// Whole-exchange deadline.
    pub exchange_timeout: Duration,
    /// Grace between interrupt and forced cleanup.
    pub interrupt_grace: Duration,
    /// Maximum JSONL event size.
    pub event_limit: usize,
    /// Maximum retained stderr size.
    pub stderr_limit: usize,
    /// Provider-specific diagnostic labels.
    pub labels: CliProcessLabels,
    /// Environment names permitted to reach the child.
    pub environment_allowlist: &'static [&'static str],
    /// Allowed variables that select the CLI credential store.
    pub credential_home_variables: &'static [&'static str],
}

/// Provider-neutral classification of an event decoding failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CliDecodeFailureClass {
    /// Provider response could not be decoded.
    ProviderDecode,
    /// The CLI violated its streaming protocol.
    StreamProtocolViolation,
}

/// A content-bearing decoder failure sanitized by the shared runner.
#[derive(Debug)]
pub struct CliDecodeFailure {
    class: CliDecodeFailureClass,
    detail: String,
}

impl CliDecodeFailure {
    /// Builds a failure from an adapter's provider-specific decoder error.
    pub fn new(class: CliDecodeFailureClass, detail: String) -> Self {
        Self { class, detail }
    }

    /// Returns the provider-neutral failure class.
    pub fn class(&self) -> CliDecodeFailureClass {
        self.class
    }

    /// Returns the provider-specific failure detail.
    pub fn detail(&self) -> &str {
        &self.detail
    }

    /// Separates the failure into its provider-neutral class and detail.
    pub fn into_parts(self) -> (CliDecodeFailureClass, String) {
        (self.class, self.detail)
    }
}

impl std::fmt::Display for CliDecodeFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.detail)
    }
}

impl std::error::Error for CliDecodeFailure {}

/// Provider-specific interpretation at the edges of the shared process loop.
pub trait CliSession<C>: Sized {
    /// Whether the decoder has observed terminal provider evidence.
    fn terminal_observed(&self) -> bool;
    /// Decodes and emits one bounded JSONL event.
    fn push(
        &mut self,
        line: &[u8],
        sink: &mut RedactingSink<'_, C>,
    ) -> Result<(), CliDecodeFailure>;
    /// Converts a sanitized decode failure into typed terminal evidence.
    fn decode_failure(self, class: CliDecodeFailureClass, detail: String) -> TerminalEvidence;
    /// Produces terminal evidence after a successful process exit.
    fn finish(self, sink: &mut RedactingSink<'_, C>) -> TerminalEvidence;
    /// Produces typed boundary-loss evidence.
    fn boundary_loss(self, cause: LossCause) -> TerminalEvidence;
    /// Preserves a provider failure while demoting an incomplete upload.
    fn boundary_loss_unless_provider_failure(
        self,
        cause: LossCause,
        sink: &mut RedactingSink<'_, C>,
    ) -> TerminalEvidence;
    /// Produces a provider failure after a non-successful process exit.
    fn provider_error_after_exit(
        self,
        message: &str,
        classification: &str,
        sink: &mut RedactingSink<'_, C>,
    ) -> TerminalEvidence;
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

/// Runs one provider CLI command to a typed terminal outcome.
pub async fn execute_cli_process<C: Clone + Send + Sync, D: CliSession<C>>(
    request: CliProcessRequest<C, D>,
    sink: &mut (dyn ObservationSink<C> + Send),
    cancellation: &mut CancellationSignal,
) -> TerminalEvidence {
    if !CLI_PROCESS_GROUP_SUPERVISION_SUPPORTED {
        return TerminalEvidence::ProvenUnsent(ProvenUnsentEvidence {
            cause: UnsentCause::ConnectFailed(TransportFacts::new(format!(
                "{} process-group supervision is unsupported on this platform",
                request.labels.process
            ))),
        });
    }
    let CliProcessRequest {
        command,
        prompt,
        correlation,
        decoder,
        terminal_text_capture,
        exchange_timeout,
        interrupt_grace,
        event_limit,
        stderr_limit,
        labels,
        environment_allowlist,
        credential_home_variables,
    } = request;
    let mut command = Command::from(command);
    command
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let environment = match allowlisted_environment(
        environment_allowlist,
        credential_home_variables,
        labels,
        |name| std::env::var_os(name),
    ) {
        Ok(environment) => environment,
        Err(rejection) => {
            // Nothing was sent: the rejection precedes `SendCommenced` and the
            // spawn itself. The diagnostic names only the variable and the
            // accurate reason, never the value.
            return TerminalEvidence::ProvenUnsent(ProvenUnsentEvidence {
                cause: UnsentCause::ConnectFailed(TransportFacts::new(rejection.diagnostic())),
            });
        }
    };
    for (name, value) in environment {
        command.env(name, value);
    }
    #[cfg(unix)]
    command.process_group(0);

    // Re-poll immediately before the irrevocable boundary: the pre-`execute`
    // check ran before the command and environment were assembled, and a
    // signal that became ready during that work must still yield ProvenUnsent
    // rather than dispatch an operation the caller already cancelled.
    if cancellation.is_cancelled() {
        return TerminalEvidence::ProvenUnsent(ProvenUnsentEvidence {
            cause: UnsentCause::CancelledBeforeSend,
        });
    }

    sink.observe(Observation {
        correlation: correlation.clone(),
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
    let Some(deadline) = tokio::time::Instant::now().checked_add(exchange_timeout) else {
        force_kill(&mut child).await;
        return pre_exchange_transport_loss(format!(
            "{} exchange timeout cannot be represented by the runtime clock",
            labels.process
        ));
    };
    let Some(mut stdin) = child.stdin.take() else {
        force_kill(&mut child).await;
        return pre_exchange_transport_loss(format!(
            "spawned {} process has no stdin",
            labels.provider
        ));
    };
    let Some(stdout) = child.stdout.take() else {
        force_kill(&mut child).await;
        return pre_exchange_transport_loss(format!(
            "spawned {} process has no stdout",
            labels.provider
        ));
    };
    let Some(stderr) = child.stderr.take() else {
        force_kill(&mut child).await;
        return pre_exchange_transport_loss(format!(
            "spawned {} process has no stderr",
            labels.provider
        ));
    };
    let mut stderr_task =
        tokio::spawn(async move { read_bounded_output(stderr, stderr_limit).await });
    let mut decoder = decoder;
    let mut redacting_sink = RedactingSink::new(sink);
    match terminal_text_capture {
        CliTerminalTextCapture::Disabled => {}
        CliTerminalTextCapture::TerminalOnly => {
            redacting_sink.begin_terminal_only_text_capture();
        }
        CliTerminalTextCapture::StreamAndTerminal => {
            redacting_sink.begin_streaming_terminal_text_capture();
        }
    }
    let input_step = {
        let send_prompt = async {
            stdin.write_all(&prompt).await?;
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
                Some(std::io::Error::other(format!(
                    "request upload cancelled after the {} leader had exited",
                    labels.process
                )))
            } else {
                interrupt_then_kill(
                    &mut child,
                    remaining_interrupt_grace(interrupt_grace, deadline),
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
                Some(std::io::Error::other(format!(
                    "request upload timed out after the {} leader had exited",
                    labels.process
                )))
            } else {
                force_kill(&mut child).await;
                abort_stderr_task(&mut stderr_task).await;
                return pre_exchange_boundary_loss(timeout_cause(labels));
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
            result = read_bounded_line(&mut stdout, event_limit, labels) => ProcessStep::Line(result),
            () = &mut *cancellation => ProcessStep::Cancelled,
            () = tokio::time::sleep_until(deadline) => ProcessStep::TimedOut,
        };
        match next {
            ProcessStep::Line(Ok(Some(line))) => {
                if let Err(error) = decoder.push(&line, &mut redacting_sink) {
                    let (class, error_detail) = error.into_parts();
                    // Serde details quote provider-controlled bytes, and both
                    // that library's prose and the adapter's own wrapper sit
                    // between a held credential marker and the continuation the
                    // detail quotes — so a joined-form scan reads the join as
                    // clean however the pieces are ordered. The detail is
                    // therefore content-silent whenever any context is held,
                    // and keeps its content only when nothing could complete.
                    let detail = format!(
                        "undecodable {}: {}",
                        labels.decode_event,
                        redacting_sink.redact_wrapped_provider_detail(&error_detail)
                    );
                    force_kill(&mut child).await;
                    abort_stderr_task(&mut stderr_task).await;
                    redacting_sink.finish();
                    return decoder.decode_failure(class, detail);
                }
                if !decoder.terminal_observed() && cancellation.is_cancelled() {
                    // Work-first: an already-exited leader's status is
                    // definitive and outranks a simultaneous cancellation.
                    if let Some((status, detail)) =
                        reap_exited_leader(&mut child, &mut stderr_task, labels).await
                    {
                        reaped_status = Some(status);
                        deadline_stderr = Some(detail);
                        break;
                    }
                    interrupt_then_kill(
                        &mut child,
                        remaining_interrupt_grace(interrupt_grace, deadline),
                    )
                    .await;
                    abort_stderr_task(&mut stderr_task).await;
                    redacting_sink.finish();
                    return decoder.boundary_loss(LossCause::CancellationRequested);
                }
                // Rechecked after every decoded line — never gated on an
                // empty reader buffer: a descendant continuously filling
                // stdout keeps the biased read arm always ready AND the
                // buffer non-empty, which would otherwise starve the deadline
                // (and the cancellation check above) indefinitely.
                if !decoder.terminal_observed() && tokio::time::Instant::now() >= deadline {
                    // Work-first, as in the cancellation arm: a leader that
                    // already exited on its own is definitive evidence this
                    // synchronous deadline check must not discard — a nonzero
                    // exit classifies as a provider error, and a successful
                    // exit without a terminal marker stays typed exit
                    // evidence, rather than either becoming timeout loss.
                    if let Some((status, detail)) =
                        reap_exited_leader(&mut child, &mut stderr_task, labels).await
                    {
                        reaped_status = Some(status);
                        deadline_stderr = Some(detail);
                        break;
                    }
                    force_kill(&mut child).await;
                    abort_stderr_task(&mut stderr_task).await;
                    redacting_sink.finish();
                    return decoder.boundary_loss(timeout_cause(labels));
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
                    reap_exited_leader(&mut child, &mut stderr_task, labels).await
                {
                    reaped_status = Some(status);
                    deadline_stderr = Some(detail);
                    break;
                }
                interrupt_then_kill(
                    &mut child,
                    remaining_interrupt_grace(interrupt_grace, deadline),
                )
                .await;
                abort_stderr_task(&mut stderr_task).await;
                // A cancellation that arrives after the terminal marker
                // drives cleanup but cannot replace the definitive evidence,
                // exactly as on the stderr wait below.
                if terminal_observed {
                    let evidence = if let Some(error) = input_error {
                        decoder.boundary_loss_unless_provider_failure(
                            incomplete_upload_cause(&error, labels),
                            &mut redacting_sink,
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
                            &format!(
                                "{} stderr was unavailable at the process-cleanup deadline",
                                labels.provider
                            ),
                            labels,
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
                        return decoder.boundary_loss(timeout_cause(labels));
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
        result = &mut stderr_task => stderr_result(result, labels),
        () = &mut *cancellation => {
            // Probe first: an already-exited leader's status is definitive and
            // outranks a cancellation even after a terminal marker or while a
            // descendant holds stderr open — a nonzero exit must classify as a
            // provider error rather than let cancellation launder a failed
            // invocation into the recorded completion.
            if let Some((status, detail)) =
                reap_exited_leader(&mut child, &mut stderr_task, labels).await
            {
                reaped_status = Some(status);
                detail
            } else if !terminal_observed {
                interrupt_then_kill(
                    &mut child,
                    remaining_interrupt_grace(interrupt_grace, deadline),
                )
                .await;
                abort_stderr_task(&mut stderr_task).await;
                redacting_sink.finish();
                return decoder.boundary_loss(LossCause::CancellationRequested);
            } else {
                let cleanup_grace = remaining_interrupt_grace(interrupt_grace, deadline);
                interrupt_then_kill(&mut child, cleanup_grace).await;
                abort_stderr_task(&mut stderr_task).await;
                // Cancellation does not launder an incomplete request upload: a
                // nominal completion still demotes to boundary loss exactly as
                // on the normal exit path below, because the adapter cannot
                // prove the full authorized frontier reached the CLI.
                let evidence = if let Some(error) = input_error {
                    decoder.boundary_loss_unless_provider_failure(
                        incomplete_upload_cause(&error, labels),
                        &mut redacting_sink,
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
                        &format!(
                            "{} stderr was unavailable at the process-cleanup deadline",
                            labels.provider
                        ),
                        labels,
                    )
                    .await
                }
                Ok(Some(_)) | Ok(None) | Err(_) => {
                    force_kill(&mut child).await;
                    abort_stderr_task(&mut stderr_task).await;
                    redacting_sink.finish();
                    return decoder.boundary_loss(timeout_cause(labels));
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
                let exit_wait = wait_for_exit_without_reaping(child.id(), labels);
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
                                remaining_interrupt_grace(interrupt_grace, deadline),
                            )
                            .await;
                            // A cancellation after a terminal marker drives
                            // cleanup but cannot replace the definitive
                            // evidence, exactly as on the stdout and stderr
                            // waits above.
                            if terminal_observed {
                                let evidence = if let Some(error) = input_error {
                                    decoder.boundary_loss_unless_provider_failure(
                                        incomplete_upload_cause(&error, labels),
                                        &mut redacting_sink,
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
                            return decoder.boundary_loss(timeout_cause(labels));
                        }
                    },
                }
            };
            if let Err(error) = exit_ready {
                force_kill(&mut child).await;
                redacting_sink.finish();
                return decoder.boundary_loss(LossCause::TransportFailed(TransportFacts::new(
                    format!(
                        "could not observe {} process exit safely: {error}",
                        labels.process
                    ),
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
                    incomplete_upload_cause(&error, labels),
                    &mut redacting_sink,
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
            let stderr_detail = sanitized_stderr(&redacting_sink, &stderr, stderr_limit);
            // The emitted message carries only sanitized stderr; the failure
            // is classified from the bounded raw stderr so an explicit error
            // phrase sharing a line with a consumed credential marker still
            // reaches the classifier.
            let (message, classification) = if !stderr_detail.trim().is_empty() {
                (
                    format!(
                        "{} exited with status {status}: {stderr_detail}",
                        labels.process
                    ),
                    format!(
                        "{} exited with status {status}: {}",
                        labels.process,
                        stderr.classification.trim()
                    ),
                )
            } else if let Some(error) = input_error {
                let message = format!(
                    "{} exited with status {status} after stdin failed: {error}",
                    labels.process
                );
                (message.clone(), message)
            } else {
                let message = format!("{} exited with status {status}", labels.process);
                (message.clone(), message)
            };
            // Evidence is built before the sink flushes so the failure
            // message still sees the held cross-fragment redaction state.
            let evidence =
                decoder.provider_error_after_exit(&message, &classification, &mut redacting_sink);
            redacting_sink.finish();
            evidence
        }
        Err(error) => {
            redacting_sink.finish();
            decoder.boundary_loss(LossCause::TransportFailed(TransportFacts::new(format!(
                "could not wait for {} process: {error}",
                labels.process
            ))))
        }
    }
}

/// `CODEX_HOME` names the CLI's login store — with `HOME` supplying its
/// `$HOME/.codex` fallback — and the child interprets both after
/// `current_dir` moves it to the configured working root, so a relative
/// operator value would silently re-root the credential store beneath the
/// working directory. Absolutize both against the parent's own current
/// directory; every other allowlisted value passes through unchanged.
/// The parent's own absolutization, named so the injectable
/// [`absolute_credential_home`] receives a higher-ranked function item rather
/// than one lifetime instantiation of the generic `std::path::absolute`.
fn absolutize_against_current_directory(
    path: &std::path::Path,
) -> std::io::Result<std::path::PathBuf> {
    std::path::absolute(path)
}

/// The absolute form of a `HOME`/`CODEX_HOME` value, resolved through
/// `absolutize` against the parent's own current directory — `None` when it
/// cannot be made absolute at all.
///
/// An empty value absolutizes to that current directory and would point the
/// child's login store at `<cwd>/.codex`; a relative value whose absolutization
/// fails — the parent's current directory was deleted, so there is nothing to
/// resolve against — would otherwise reach the child verbatim and be re-rooted
/// under the configured working directory. Both select an unintended ambient
/// credential store, so `environment_rejection` refuses both before spawn.
fn absolute_credential_home(
    value: &std::ffi::OsStr,
    absolutize: impl Fn(&std::path::Path) -> std::io::Result<std::path::PathBuf>,
) -> Option<std::ffi::OsString> {
    if value.is_empty() {
        return None;
    }
    absolutize(std::path::Path::new(value))
        .ok()
        .filter(|path| path.is_absolute())
        .map(std::path::PathBuf::into_os_string)
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
/// rejecting (with the offending variable's name, never its value): a proxy URL
/// that embeds userinfo — such a credential would transit to the child verbatim
/// and a CLI that reflects its proxy configuration would hand the password to
/// output the adapter can only shape-redact, and `redact_text` has no
/// proxy-userinfo rule (INV-035) — and a `HOME`/`CODEX_HOME` the parent cannot
/// resolve to an absolute directory, which would point the child's credential
/// store somewhere under its working directory and select an unintended ambient
/// login (see [`absolute_credential_home`]). Both must never reach the child.
fn allowlisted_environment(
    allowlist: &'static [&'static str],
    credential_home_variables: &'static [&'static str],
    labels: CliProcessLabels,
    read: impl Fn(&str) -> Option<std::ffi::OsString>,
) -> Result<Vec<(&'static str, std::ffi::OsString)>, EnvironmentRejection> {
    let mut environment = Vec::new();
    for name in allowlist {
        if let Some(value) = read(name) {
            match prepared_environment_value(name, value, credential_home_variables) {
                Ok(prepared) => environment.push((*name, prepared)),
                Err(reason) => {
                    return Err(EnvironmentRejection {
                        name,
                        reason,
                        labels,
                    });
                }
            }
        }
    }
    Ok(environment)
}

/// A refused environment variable: the variable name (never its value) and the
/// exact reason, so the reference-only diagnostic names the correct remediation.
#[derive(Debug)]
struct EnvironmentRejection {
    name: &'static str,
    reason: EnvironmentRejectionReason,
    labels: CliProcessLabels,
}

#[derive(Debug, PartialEq, Eq)]
enum EnvironmentRejectionReason {
    /// A proxy URL authority embeds userinfo (`scheme://user:secret@host`).
    EmbedsUserinfo,
    /// A proxy value is not UTF-8, so it cannot be verified credential-free.
    Unverifiable,
    /// `HOME`/`CODEX_HOME` is present but cannot be resolved to an absolute
    /// directory, so the child would select an ambient credential store under
    /// its own working directory.
    UnresolvableCredentialHome,
}

impl EnvironmentRejection {
    fn diagnostic(&self) -> String {
        let name = self.name;
        match self.reason {
            EnvironmentRejectionReason::EmbedsUserinfo => format!(
                "inherited `{name}` embeds URL userinfo; the {} would receive that \
                 credential verbatim and could reflect it in output the adapter can only \
                 shape-redact, so the exchange is refused — remove the credential from the \
                 proxy URL",
                self.labels.process
            ),
            EnvironmentRejectionReason::Unverifiable => format!(
                "inherited `{name}` is not valid UTF-8 and cannot be verified free of embedded \
                 credentials, so the exchange is refused — set it to a valid UTF-8 proxy URL"
            ),
            EnvironmentRejectionReason::UnresolvableCredentialHome => format!(
                "inherited `{name}` cannot be resolved to an absolute directory — it is empty, \
                 or the working directory it would resolve against is gone — so the {} login \
                 store would land under the child's own working directory and select an \
                 unintended ambient login, and the exchange is refused — set it to an absolute \
                 directory or leave it unset",
                self.labels.provider
            ),
        }
    }
}

/// The value the child receives for one allowlisted variable, or why it is
/// refused.
///
/// A credential home is absolutized exactly once here, and the value that
/// passed validation is the value assembled: resolving it a second time during
/// assembly would let a parent working directory that became unresolvable in
/// between — the very case this boundary exists for — fail the second
/// resolution and forward the original relative path, which the child would
/// then re-root beneath its own working directory.
fn prepared_environment_value(
    name: &str,
    value: std::ffi::OsString,
    credential_home_variables: &[&str],
) -> Result<std::ffi::OsString, EnvironmentRejectionReason> {
    if credential_home_variables.contains(&name) {
        return absolute_credential_home(&value, absolutize_against_current_directory)
            .ok_or(EnvironmentRejectionReason::UnresolvableCredentialHome);
    }
    match proxy_rejection(name, &value) {
        Some(reason) => Err(reason),
        None => Ok(value),
    }
}

/// Why a proxy variable's value is refused, or `None` when it is acceptable.
/// The authority is the span after any `scheme://` up to the first `/`, `?`, or
/// `#`; userinfo is an `@` inside it, and a value that cannot be read as UTF-8
/// cannot be verified credential-free and fails closed as `Unverifiable` — a
/// distinct reason from an actual embedded credential. A variable that is not a
/// proxy URL is not checked.
fn proxy_rejection(name: &str, value: &std::ffi::OsStr) -> Option<EnvironmentRejectionReason> {
    if !PROXY_URL_VARIABLES.contains(&name) {
        return None;
    }
    let Some(text) = value.to_str() else {
        return Some(EnvironmentRejectionReason::Unverifiable);
    };
    let after_scheme = text.split_once("://").map_or(text, |(_, rest)| rest);
    let authority = after_scheme.split(['/', '?', '#']).next().unwrap_or("");
    authority
        .contains('@')
        .then_some(EnvironmentRejectionReason::EmbedsUserinfo)
}

/// Reaps a leader that has already exited on its own, returning its status and
/// stderr detail so a cancellation racing that exit hands the definitive
/// provider-error evidence to the exit-classification path instead of
/// reporting cancellation loss. `None` when the leader is still running.
async fn reap_exited_leader(
    child: &mut SupervisedChild,
    stderr_task: &mut tokio::task::JoinHandle<std::io::Result<BoundedOutput>>,
    labels: CliProcessLabels,
) -> Option<(std::io::Result<std::process::ExitStatus>, BoundedOutput)> {
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
        &format!(
            "{} stderr was unavailable after cancellation",
            labels.provider
        ),
        labels,
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
    stderr_task: &mut tokio::task::JoinHandle<std::io::Result<BoundedOutput>>,
    unavailable_message: &str,
    labels: CliProcessLabels,
) -> BoundedOutput {
    match tokio::time::timeout(POST_KILL_REAP_BOUND, &mut *stderr_task).await {
        Ok(result) => stderr_result(result, labels),
        Err(_) => {
            abort_stderr_task(stderr_task).await;
            BoundedOutput::diagnostic(unavailable_message.to_string())
        }
    }
}

fn incomplete_upload_cause(error: &std::io::Error, labels: CliProcessLabels) -> LossCause {
    LossCause::TransportFailed(TransportFacts::new(format!(
        "{} stdin closed before the full request upload completed: {error}",
        labels.provider
    )))
}

fn stderr_result(
    result: Result<std::io::Result<BoundedOutput>, tokio::task::JoinError>,
    labels: CliProcessLabels,
) -> BoundedOutput {
    match result {
        Ok(Ok(stderr)) => stderr,
        Ok(Err(error)) => BoundedOutput::diagnostic(format!(
            "could not read {} stderr: {error}",
            labels.provider
        )),
        Err(error) => {
            BoundedOutput::diagnostic(format!("{} stderr reader failed: {error}", labels.provider))
        }
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
    labels: CliProcessLabels,
) -> std::io::Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    // Payload bytes counted so far. Tracked apart from `line.len()` because a
    // `\r` ending a batch is only a delimiter byte once the next batch shows
    // whether a `\n` follows it, so its charge is deferred rather than counted
    // and then unaccounted for.
    let mut counted = 0_usize;
    let mut deferred_carriage_return = false;
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            // End of stream resolves a deferred `\r`: no `\n` can follow it, so
            // it was payload rather than half a delimiter. It is still attached
            // to the line the decoder receives, so charge it before admitting.
            if deferred_carriage_return && counted.saturating_add(1) > limit {
                return Err(oversize_event(limit, labels));
            }
            return if line.is_empty() {
                Ok(None)
            } else {
                Ok(Some(line))
            };
        }
        // The deferred `\r` is delimiter only when this batch opens with the
        // `\n` that completes the pair; otherwise it was payload after all.
        if deferred_carriage_return {
            if available[0] != b'\n' {
                counted += 1;
            }
            deferred_carriage_return = false;
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let take = newline.map_or(available.len(), |index| index + 1);
        // Only the payload counts toward the event limit — the line delimiter
        // (`\n`, or a `\r\n` pair) is stripped before decoding, so including it
        // would reject an exactly-`limit`-byte event in ordinary
        // newline-terminated JSONL while admitting the same event unterminated
        // at EOF. The exclusion holds however the reader chunks the stream: a
        // `\r\n` split across two batches costs the event nothing, exactly as
        // an unsplit one does.
        let payload = match newline {
            Some(index) if index > 0 && available[index - 1] == b'\r' => index - 1,
            Some(index) => index,
            None if available.last() == Some(&b'\r') => {
                deferred_carriage_return = true;
                available.len() - 1
            }
            None => available.len(),
        };
        if counted.saturating_add(payload) > limit {
            return Err(oversize_event(limit, labels));
        }
        counted += payload;
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

fn oversize_event(limit: usize, labels: CliProcessLabels) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!("{} exceeded the {limit}-byte limit", labels.bounded_event),
    )
}

struct BoundedOutput {
    text: String,
    classification: String,
    evidence_truncated: bool,
}

impl BoundedOutput {
    fn diagnostic(text: String) -> Self {
        Self {
            classification: text.clone(),
            text,
            evidence_truncated: false,
        }
    }
}

async fn read_bounded_output<R: AsyncRead + Unpin>(
    mut reader: R,
    evidence_limit: usize,
) -> std::io::Result<BoundedOutput> {
    let sanitization_limit = stderr_sanitization_limit(evidence_limit);
    let mut retained = Vec::with_capacity(sanitization_limit.min(8192));
    let mut buffer = [0_u8; 8192];
    let mut evidence_truncated = false;
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let admitted = sanitization_limit.saturating_sub(retained.len()).min(read);
        retained.extend_from_slice(&buffer[..admitted]);
        evidence_truncated |= retained.len() > evidence_limit || admitted < read;
    }
    let classification_end = retained.len().min(evidence_limit);
    let mut classification = String::from_utf8_lossy(&retained[..classification_end]).into_owned();
    if evidence_truncated {
        classification.push_str(TRUNCATION_SUFFIX);
    }
    Ok(BoundedOutput {
        text: String::from_utf8_lossy(&retained).into_owned(),
        classification,
        evidence_truncated,
    })
}

fn stderr_sanitization_limit(evidence_limit: usize) -> usize {
    evidence_limit.saturating_mul(2)
}

fn sanitized_stderr<C: Clone>(
    sink: &RedactingSink<'_, C>,
    stderr: &BoundedOutput,
    evidence_limit: usize,
) -> String {
    // The reader retains an additional evidence-limit-sized window so
    // JSON-aware sanitization can see escapes and closing syntax beyond the
    // emitted prefix. Cutting at the evidence limit first could split an
    // escape and hide the reversible credential it encodes.
    let sanitized = sink.redact_terminal_failure_text(&stderr.text);
    truncate_text(&sanitized, evidence_limit, stderr.evidence_truncated)
}

fn truncate_text(text: &str, limit: usize, force_suffix: bool) -> String {
    if !force_suffix && text.len() <= limit {
        return text.to_string();
    }
    let mut end = text.len().min(limit);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{TRUNCATION_SUFFIX}", &text[..end])
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
async fn wait_for_exit_without_reaping(
    process_group_id: Option<u32>,
    labels: CliProcessLabels,
) -> std::io::Result<()> {
    let raw_pid = process_group_id.ok_or_else(|| {
        std::io::Error::other(format!(
            "spawned {} process has no process-group identity",
            labels.provider
        ))
    })?;
    let pid = rustix::process::Pid::from_raw(raw_pid as i32).ok_or_else(|| {
        std::io::Error::other(format!(
            "spawned {} process has an invalid process-group identity",
            labels.provider
        ))
    })?;
    tokio::task::spawn_blocking(move || {
        let status = rustix::process::waitid(
            rustix::process::WaitId::Pid(pid),
            rustix::process::WaitIdOptions::EXITED | rustix::process::WaitIdOptions::NOWAIT,
        )
        .map_err(std::io::Error::from)?;
        status.map(drop).ok_or_else(|| {
            std::io::Error::other(format!(
                "{} process exit was not observable",
                labels.provider
            ))
        })
    })
    .await
    .map_err(|error| {
        std::io::Error::other(format!("{} exit waiter failed: {error}", labels.provider))
    })?
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
async fn wait_for_exit_without_reaping(
    _process_group_id: Option<u32>,
    _labels: CliProcessLabels,
) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "non-reaping process wait is unavailable",
    ))
}

async fn abort_stderr_task(
    stderr_task: &mut tokio::task::JoinHandle<std::io::Result<BoundedOutput>>,
) {
    stderr_task.abort();
    let _ = stderr_task.await;
}

fn pre_exchange_transport_loss(detail: impl Into<String>) -> TerminalEvidence {
    pre_exchange_boundary_loss(LossCause::TransportFailed(TransportFacts::new(detail)))
}

fn timeout_cause(labels: CliProcessLabels) -> LossCause {
    LossCause::TimedOut(TransportFacts::new(format!(
        "{} process exceeded its exchange timeout",
        labels.process
    )))
}

fn pre_exchange_boundary_loss(cause: LossCause) -> TerminalEvidence {
    TerminalEvidence::BoundaryLoss(crate::BoundaryLossEvidence {
        cause,
        exchange: crate::ExchangeFacts::default(),
        reported_model: None,
        finish_reported: None,
        usage: crate::TokenUsage::unreported(),
    })
}
#[cfg(test)]
mod tests {
    use super::{
        BoundedOutput, CliProcessLabels, EnvironmentRejection, EnvironmentRejectionReason,
        TRUNCATION_SUFFIX, absolute_credential_home, allowlisted_environment, read_bounded_line,
        read_bounded_output, sanitized_stderr,
    };
    use crate::{REDACTED, RedactingSink};

    const TEST_ENVIRONMENT_ALLOWLIST: &[&str] = &[
        "ALL_PROXY",
        "CLAUDE_CONFIG_DIR",
        "CODEX_HOME",
        "HOME",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "NO_PROXY",
        "PATH",
        "TERM",
        "http_proxy",
    ];
    const TEST_CREDENTIAL_HOME_VARIABLES: &[&str] = &["HOME", "CODEX_HOME", "CLAUDE_CONFIG_DIR"];
    const TEST_LABELS: CliProcessLabels = CliProcessLabels {
        provider: "Codex",
        process: "Codex CLI",
        decode_event: "Codex event",
        bounded_event: "Codex JSONL event",
    };

    #[test]
    fn stderr_is_sanitized_before_the_evidence_limit_is_applied() {
        const SYNTHETIC_CREDENTIAL_PREFIX: &str = "SYNTHETIC-SECRET-STDERR-";
        let synthetic_credential = format!("{SYNTHETIC_CREDENTIAL_PREFIX}Z");
        let escaped_credential = format!(r"{SYNTHETIC_CREDENTIAL_PREFIX}\u005a");
        let body = format!(
            r#"{{"padding":"{}","api_key":"{escaped_credential}","tail":"{}"}}"#,
            "x".repeat(96),
            "y".repeat(200)
        );
        let evidence_limit = body
            .find(r"\u")
            .expect("the fixture carries one JSON escape")
            + 2;
        let stderr = BoundedOutput {
            classification: body.clone(),
            text: body,
            evidence_truncated: false,
        };
        let mut observed: Vec<crate::Observation<u8>> = Vec::new();
        let sink = RedactingSink::new(&mut observed);

        let sanitized = sanitized_stderr(&sink, &stderr, evidence_limit);

        assert!(sanitized.contains(REDACTED));
        assert!(sanitized.ends_with(TRUNCATION_SUFFIX));
        assert!(!sanitized.contains(&synthetic_credential));
        assert!(!sanitized.contains(&escaped_credential));
    }

    #[test]
    fn an_incomplete_stderr_window_is_sanitized_and_marked_truncated() {
        const SYNTHETIC_CREDENTIAL: &str = "SYNTHETIC-SECRET-STDERR-Z";
        let stderr = BoundedOutput {
            text: format!("api_key={SYNTHETIC_CREDENTIAL}"),
            classification: String::new(),
            evidence_truncated: true,
        };
        let mut observed: Vec<crate::Observation<u8>> = Vec::new();
        let sink = RedactingSink::new(&mut observed);

        let sanitized = sanitized_stderr(&sink, &stderr, usize::MAX);

        assert_eq!(sanitized, format!("api_key={REDACTED}{TRUNCATION_SUFFIX}"));
        assert!(!sanitized.contains(SYNTHETIC_CREDENTIAL));
    }

    #[tokio::test]
    async fn stderr_reader_retains_one_extra_evidence_window_for_sanitization() {
        const EVIDENCE_LIMIT: usize = 8;
        const INPUT: &[u8] = b"abcdefghijklmnopq";
        const RETAINED_SANITIZATION_WINDOW: &[u8] = b"abcdefghijklmnop";

        let output = read_bounded_output(INPUT, EVIDENCE_LIMIT)
            .await
            .expect("the in-memory reader succeeds");

        assert_eq!(output.text.as_bytes(), RETAINED_SANITIZATION_WINDOW);
    }

    #[tokio::test]
    async fn stderr_classification_preserves_the_original_bounded_prefix() {
        const EVIDENCE_LIMIT: usize = 8;
        const INPUT: &[u8] = b"abcdefghijklmnopq";
        const EXPECTED_CLASSIFICATION: &str = "abcdefgh… [truncated]";

        let output = read_bounded_output(INPUT, EVIDENCE_LIMIT)
            .await
            .expect("the in-memory reader succeeds");

        assert_eq!(output.classification, EXPECTED_CLASSIFICATION);
    }

    #[tokio::test]
    async fn stderr_reader_honors_limits_above_the_response_body_default() {
        const EVIDENCE_LIMIT: usize = crate::MAX_BUFFERED_PROVIDER_RESPONSE_BYTES + 1;
        let input = vec![b'x'; EVIDENCE_LIMIT];

        let output = read_bounded_output(input.as_slice(), EVIDENCE_LIMIT)
            .await
            .expect("the in-memory reader succeeds");

        assert_eq!(output.text.as_bytes(), input);
        assert!(!output.evidence_truncated);
    }

    #[test]
    fn codex_home_is_absolutized_for_the_child() {
        let value = accepted_value("CODEX_HOME", std::ffi::OsString::from("relative-home"));

        assert!(std::path::Path::new(&value).is_absolute());
    }

    #[test]
    fn claude_config_dir_is_absolutized_for_the_child() {
        let value = accepted_value(
            "CLAUDE_CONFIG_DIR",
            std::ffi::OsString::from("relative-home"),
        );

        assert!(std::path::Path::new(&value).is_absolute());
    }

    #[test]
    fn fallback_home_is_absolutized_for_the_child() {
        let value = accepted_value("HOME", std::ffi::OsString::from("relative-home"));

        assert!(std::path::Path::new(&value).is_absolute());
    }

    #[test]
    fn other_allowlisted_environment_values_pass_through_unchanged() {
        assert_eq!(
            accepted_value("PATH", std::ffi::OsString::from("relative:paths")),
            std::ffi::OsString::from("relative:paths")
        );
    }

    /// Assembles the environment with exactly `name=value` set and returns the
    /// rejection, so each test asserts the reason/name straight-line. The
    /// branching lives here in the helper, not in a test body.
    #[track_caller]
    fn rejection_for(name: &'static str, value: std::ffi::OsString) -> EnvironmentRejection {
        allowlisted_environment(
            TEST_ENVIRONMENT_ALLOWLIST,
            TEST_CREDENTIAL_HOME_VARIABLES,
            TEST_LABELS,
            |queried| (queried == name).then(|| value.clone()),
        )
        .expect_err("the value must be rejected")
    }

    /// Assembles the environment with exactly `name=value` set and returns the
    /// assembled child value, so a passing case asserts equality straight-line.
    #[track_caller]
    fn accepted_value(name: &'static str, value: std::ffi::OsString) -> std::ffi::OsString {
        let env = allowlisted_environment(
            TEST_ENVIRONMENT_ALLOWLIST,
            TEST_CREDENTIAL_HOME_VARIABLES,
            TEST_LABELS,
            |queried| (queried == name).then(|| value.clone()),
        )
        .expect("the value must pass through");
        env.into_iter()
            .find(|(assembled, _)| *assembled == name)
            .expect("the assembled environment contains the variable")
            .1
    }

    /// Credential redaction: a proxy URL embedding authority userinfo is refused as such.
    #[test]
    fn credential_bearing_proxy_url_is_rejected() {
        let rejection = rejection_for(
            "HTTP_PROXY",
            "http://alice:opaque-proxy-value@proxy.internal:8080".into(),
        );

        assert_eq!(rejection.name, "HTTP_PROXY");
        assert_eq!(rejection.reason, EnvironmentRejectionReason::EmbedsUserinfo);
    }

    /// Credential redaction: username-only userinfo is still an inherited secret shape.
    #[test]
    fn password_less_proxy_userinfo_is_rejected() {
        let rejection = rejection_for("HTTPS_PROXY", "https://alice@proxy.internal".into());

        assert_eq!(rejection.reason, EnvironmentRejectionReason::EmbedsUserinfo);
    }

    /// Credential redaction: a schemeless proxy value with userinfo is refused too.
    #[test]
    fn schemeless_proxy_userinfo_is_rejected() {
        let rejection = rejection_for(
            "ALL_PROXY",
            "alice:opaque-proxy-value@proxy.internal:8080".into(),
        );

        assert_eq!(rejection.reason, EnvironmentRejectionReason::EmbedsUserinfo);
    }

    /// Credential redaction: the lowercase variable spellings share the refusal.
    #[test]
    fn lowercase_proxy_userinfo_is_rejected() {
        let rejection = rejection_for(
            "http_proxy",
            "socks5://alice:opaque-proxy-value@proxy.internal".into(),
        );

        assert_eq!(rejection.reason, EnvironmentRejectionReason::EmbedsUserinfo);
    }

    /// A credential-free proxy URL passes through unchanged.
    #[test]
    fn credential_free_proxy_url_passes_through() {
        assert_eq!(
            accepted_value("HTTP_PROXY", "http://proxy.internal:8080".into()),
            std::ffi::OsString::from("http://proxy.internal:8080")
        );
    }

    /// An `@` confined to the URL path is not authority userinfo.
    #[test]
    fn path_confined_at_sign_passes_through() {
        assert_eq!(
            accepted_value("HTTPS_PROXY", "https://proxy.internal/path/we@ird".into()),
            std::ffi::OsString::from("https://proxy.internal/path/we@ird")
        );
    }

    /// `NO_PROXY` is a host list, never a URL with an authority.
    #[test]
    fn no_proxy_host_list_passes_through() {
        assert_eq!(
            accepted_value("NO_PROXY", "internal,@odd-but-not-a-url".into()),
            std::ffi::OsString::from("internal,@odd-but-not-a-url")
        );
    }

    /// Non-proxy allowlisted variables are not URL-checked.
    #[test]
    fn non_proxy_variables_are_not_url_checked() {
        assert_eq!(
            accepted_value("TERM", "user:secret@not-a-proxy-variable".into()),
            std::ffi::OsString::from("user:secret@not-a-proxy-variable")
        );
    }

    /// A proxy value that is not UTF-8 is refused as `Unverifiable` (the UTF-8
    /// remediation), not as an embedded-userinfo credential.
    #[cfg(unix)]
    #[test]
    fn non_utf8_proxy_value_is_rejected_as_unverifiable() {
        use std::os::unix::ffi::OsStringExt;

        let rejection = rejection_for(
            "HTTP_PROXY",
            std::ffi::OsString::from_vec(vec![0x68, 0x74, 0xff, 0xfe]),
        );

        assert_eq!(rejection.name, "HTTP_PROXY");
        assert_eq!(rejection.reason, EnvironmentRejectionReason::Unverifiable);
        assert!(rejection.diagnostic().contains("not valid UTF-8"));
        assert!(!rejection.diagnostic().contains("userinfo"));
    }

    /// An explicitly empty `HOME`/`CODEX_HOME` is refused before spawning
    /// rather than delegated to the CLI: absolutizing it would point the login
    /// store at the working directory and select an unintended ambient login.
    #[test]
    fn empty_home_is_rejected() {
        let rejection = rejection_for("HOME", std::ffi::OsString::new());

        assert_eq!(
            rejection.reason,
            EnvironmentRejectionReason::UnresolvableCredentialHome
        );
    }

    #[test]
    fn empty_codex_home_is_rejected() {
        let rejection = rejection_for("CODEX_HOME", std::ffi::OsString::new());

        assert_eq!(
            rejection.reason,
            EnvironmentRejectionReason::UnresolvableCredentialHome
        );
    }

    /// Credential redaction: a relative credential home the parent cannot absolutize — its
    /// own current directory deleted, so there is nothing to resolve against —
    /// yields no child value, which is what makes `environment_rejection`
    /// refuse it before spawn instead of forwarding the relative path for the
    /// child to re-root beneath its configured working directory.
    #[test]
    fn relative_credential_home_without_a_current_directory_is_unresolvable() {
        let resolved = absolute_credential_home(std::ffi::OsStr::new("relative-home"), |_| {
            Err(std::io::Error::from(std::io::ErrorKind::NotFound))
        });

        assert_eq!(resolved, None);
    }

    /// A credential home that passed validation is the value assembled, not a
    /// second independent resolution: `allowlisted_environment` never yields a
    /// relative credential home, whatever happens to the parent's working
    /// directory between two would-be resolutions.
    #[test]
    fn assembled_credential_home_is_never_relative() {
        let assembled = allowlisted_environment(
            TEST_ENVIRONMENT_ALLOWLIST,
            TEST_CREDENTIAL_HOME_VARIABLES,
            TEST_LABELS,
            |queried| (queried == "CODEX_HOME").then(|| std::ffi::OsString::from("relative-home")),
        )
        .expect("a resolvable relative credential home is accepted");

        assert!(
            assembled
                .iter()
                .all(|(_, value)| std::path::Path::new(value).is_absolute())
        );
    }

    /// A relative credential home the parent *can* absolutize yields the
    /// resolved absolute value, so the ordinary case still reaches the child
    /// re-rooted at the parent's own directory rather than being refused.
    #[test]
    fn relative_credential_home_resolves_against_the_current_directory() {
        let resolved = absolute_credential_home(std::ffi::OsStr::new("relative-home"), |path| {
            Ok(std::path::Path::new("/parent-working-root").join(path))
        });

        assert_eq!(
            resolved,
            Some(std::ffi::OsString::from(
                "/parent-working-root/relative-home"
            ))
        );
    }

    #[tokio::test]
    async fn bounded_line_rejects_an_unterminated_oversize_event() {
        let input = b"12345".as_slice();
        let mut reader = tokio::io::BufReader::new(input);

        let error = read_bounded_line(&mut reader, 4, TEST_LABELS)
            .await
            .expect_err("the fifth byte exceeds the configured event bound");

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    /// A payload of exactly the limit followed by a newline is admitted: the
    /// delimiter does not count toward the event limit, so an exactly-sized
    /// event is accepted in ordinary newline-terminated JSONL just as it is at
    /// EOF without a delimiter.
    #[tokio::test]
    async fn bounded_line_admits_an_exactly_sized_payload_with_a_delimiter() {
        let input = b"1234\n".as_slice();
        let mut reader = tokio::io::BufReader::new(input);

        let line = read_bounded_line(&mut reader, 4, TEST_LABELS)
            .await
            .expect("an exactly-limit payload plus its delimiter is admitted");

        assert_eq!(line.as_deref(), Some(b"1234".as_slice()));
    }

    /// The `\r` of a `\r\n` delimiter also does not count toward the limit.
    #[tokio::test]
    async fn bounded_line_admits_an_exactly_sized_payload_with_crlf() {
        let input = b"1234\r\n".as_slice();
        let mut reader = tokio::io::BufReader::new(input);

        let line = read_bounded_line(&mut reader, 4, TEST_LABELS)
            .await
            .expect("an exactly-limit payload plus its CRLF delimiter is admitted");

        assert_eq!(line.as_deref(), Some(b"1234".as_slice()));
    }

    /// The delimiter exclusion does not depend on how the reader chunks the
    /// stream: a `\r\n` whose `\r` ends one batch and whose `\n` opens the next
    /// — a legal pipe read boundary — still costs the event nothing, so an
    /// exactly-limit payload is admitted rather than rejected as oversized.
    #[tokio::test]
    async fn bounded_line_admits_an_exactly_sized_payload_with_a_split_crlf() {
        let input = b"1234\r\n".as_slice();
        let mut reader = tokio::io::BufReader::with_capacity(5, input);

        let line = read_bounded_line(&mut reader, 4, TEST_LABELS)
            .await
            .expect("a CRLF split across reader batches is still a delimiter");

        assert_eq!(line.as_deref(), Some(b"1234".as_slice()));
    }

    /// A lone `\r` ending a batch is payload, not a delimiter, when the next
    /// batch does not open with `\n`: an oversize event carrying an interior
    /// carriage return is still rejected.
    #[tokio::test]
    async fn bounded_line_counts_an_interior_carriage_return_as_payload() {
        let input = b"12\r345\n".as_slice();
        let mut reader = tokio::io::BufReader::with_capacity(3, input);

        let error = read_bounded_line(&mut reader, 5, TEST_LABELS)
            .await
            .expect_err("an interior carriage return counts toward the limit");

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    /// A deferred carriage return that end-of-stream resolves as payload is
    /// charged before the unterminated event is admitted: no `\n` can follow it,
    /// and it stays attached to the line the decoder receives, so an
    /// exactly-limit payload plus that trailing `\r` is over the bound.
    #[tokio::test]
    async fn bounded_line_charges_a_deferred_carriage_return_at_end_of_stream() {
        let input = b"1234\r".as_slice();
        let mut reader = tokio::io::BufReader::with_capacity(5, input);

        let error = read_bounded_line(&mut reader, 4, TEST_LABELS)
            .await
            .expect_err("an unterminated trailing carriage return is payload");

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    /// The same trailing carriage return within the bound is still admitted.
    #[tokio::test]
    async fn bounded_line_admits_a_deferred_carriage_return_within_the_limit() {
        let input = b"1234\r".as_slice();
        let mut reader = tokio::io::BufReader::with_capacity(5, input);

        let line = read_bounded_line(&mut reader, 5, TEST_LABELS)
            .await
            .expect("payload plus its trailing carriage return fits the bound");

        assert_eq!(line.as_deref(), Some(b"1234\r".as_slice()));
    }

    /// A payload one byte over the limit is still rejected.
    #[tokio::test]
    async fn bounded_line_rejects_an_oversize_payload_with_a_delimiter() {
        let input = b"12345\n".as_slice();
        let mut reader = tokio::io::BufReader::new(input);

        let error = read_bounded_line(&mut reader, 4, TEST_LABELS)
            .await
            .expect_err("a payload past the limit is rejected even with a delimiter");

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }
}
