//! Provider-independent CLI process supervision and session orchestration.

use std::ffi::OsString;
use std::ops::{Deref, DerefMut};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};

use crate::{
    CancellationSignal, LossCause, Observation, ObservationFact, ObservationSink,
    ProvenUnsentEvidence, ProviderErrorKind, REDACTED, RedactingSink, TerminalEvidence,
    TransportFacts, UnsentCause,
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

/// One inherited child-environment variable and its value policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CliEnvironmentVariable {
    name: &'static str,
    credential_home: bool,
}

/// One adapter-owned child-environment value.
///
/// This type deliberately implements no diagnostic formatting so a control
/// value cannot be emitted by callers that format a complete process request.
pub struct CliEnvironmentOverride {
    name: &'static str,
    value: OsString,
    replaces_inherited: bool,
}

impl CliEnvironmentOverride {
    /// Constructs one adapter-owned non-credential control.
    pub fn new(name: &'static str, value: impl Into<OsString>) -> Self {
        Self {
            name,
            value: value.into(),
            replaces_inherited: false,
        }
    }

    /// Replaces one value whose name is already admitted by the adapter's
    /// inherited environment policy.
    pub fn replacing_inherited(name: &'static str, value: impl Into<OsString>) -> Self {
        Self {
            name,
            value: value.into(),
            replaces_inherited: true,
        }
    }

    /// Returns the fixed environment-variable name.
    pub const fn name(&self) -> &'static str {
        self.name
    }

    fn replaces_inherited(&self) -> bool {
        self.replaces_inherited
    }

    fn into_parts(self) -> (&'static str, OsString) {
        (self.name, self.value)
    }
}

impl CliEnvironmentVariable {
    /// Admits a variable with ordinary shared credential-shape checks.
    pub const fn inherited(name: &'static str) -> Self {
        Self {
            name,
            credential_home: false,
        }
    }

    /// Admits a credential-store selector only after path absolutization.
    pub const fn credential_home(name: &'static str) -> Self {
        Self {
            name,
            credential_home: true,
        }
    }

    /// Returns the inherited variable name.
    pub const fn name(self) -> &'static str {
        self.name
    }

    /// Reports whether the value selects a credential-store directory.
    pub const fn is_credential_home(self) -> bool {
        self.credential_home
    }
}

/// How the shared redactor handles provider text for terminal reconstruction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CliTerminalTextCapture {
    /// The provider decoder owns terminal text independently.
    Disabled,
    /// Capture sanitized text for terminal evidence without forwarding deltas.
    TerminalOnly,
    /// Capture sanitized text and forward the same streamed deltas.
    StreamAndTerminal,
}

/// One fully constructed provider command and its shared execution policy.
pub struct CliProcessRequest<D> {
    /// Provider-specific command arguments and working directory.
    pub command: std::process::Command,
    /// Full request body written to stdin.
    pub prompt: Vec<u8>,
    /// Provider-specific event decoder.
    pub decoder: D,
    /// Whole-exchange deadline, or no deadline when explicitly unbounded.
    pub exchange_timeout: Option<Duration>,
    /// Grace between interrupt and forced cleanup.
    pub interrupt_grace: Duration,
    /// Maximum wait after process-group cleanup for the leader or stderr
    /// reader to finish, or unbounded when explicitly configured as `none`.
    pub post_kill_reap_bound: Option<Duration>,
    /// Maximum JSONL event size.
    pub event_limit: usize,
    /// Maximum emitted stderr evidence size.
    ///
    /// Sanitization transiently retains one additional window of this size so
    /// JSON-aware redaction runs before evidence truncation.
    pub stderr_limit: usize,
    /// Environment variables permitted to reach the child, each bound to its
    /// value-handling policy.
    pub environment: &'static [CliEnvironmentVariable],
    /// Adapter-owned non-credential controls applied after ambient values.
    pub environment_overrides: Vec<CliEnvironmentOverride>,
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
    /// Provider-specific diagnostic labels bound to this decoder type.
    const LABELS: CliProcessLabels;
    /// The one correlation used for every observation in this exchange.
    fn correlation(&self) -> &C;
    /// Terminal text capture and forwarding policy owned by this decoder.
    fn terminal_text_capture(&self) -> CliTerminalTextCapture;
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
    /// Records that bytes were read off the process but never delivered.
    ///
    /// `read_bounded_line` accumulates into a local buffer, so dropping its
    /// future at a cancellation or deadline discards whatever prefix it had
    /// already consumed — including, possibly, the start of an `assistant`
    /// event carrying a tool call. The default is a no-op, correct for an
    /// adapter that never claims a negative it did not establish; an adapter
    /// whose partial facts do claim one must stop claiming it here.
    fn note_undelivered_line(&mut self) {}
    /// Boundary loss for a line the runner never delivered to the decoder.
    ///
    /// The bytes were read off the process and then rejected by the line bound,
    /// so nothing about that event was examined. An adapter whose partial facts
    /// track per-event examination must answer from that state rather than from
    /// the previous event; the default is the ordinary loss, which is correct
    /// for an adapter that never claims a negative it did not establish.
    fn undelivered_line_loss(self, cause: LossCause) -> TerminalEvidence
    where
        Self: Sized,
    {
        self.boundary_loss(cause)
    }
    /// Preserves a provider failure while demoting an incomplete upload.
    fn boundary_loss_unless_provider_failure(
        self,
        cause: LossCause,
        sink: &mut RedactingSink<'_, C>,
    ) -> TerminalEvidence;
    /// Collapses raw non-successful-exit material to a closed kind.
    fn classify_provider_error_after_exit(classification: &str) -> ProviderErrorKind;
    /// Produces a provider failure from sanitized material and its closed kind.
    fn provider_error_after_exit(
        self,
        message: &str,
        kind: ProviderErrorKind,
        sink: &mut RedactingSink<'_, C>,
    ) -> TerminalEvidence;
}

struct SupervisedChild {
    child: Child,
    process_group_id: Option<u32>,
    post_kill_reap_bound: Option<Duration>,
    armed: bool,
}

impl SupervisedChild {
    fn new(child: Child, post_kill_reap_bound: Option<Duration>) -> Self {
        let process_group_id = child.id();
        Self {
            child,
            process_group_id,
            post_kill_reap_bound,
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
    request: CliProcessRequest<D>,
    sink: &mut (dyn ObservationSink<C> + Send),
    cancellation: &mut CancellationSignal,
) -> TerminalEvidence {
    let labels = D::LABELS;
    if !CLI_PROCESS_GROUP_SUPERVISION_SUPPORTED {
        return TerminalEvidence::ProvenUnsent(ProvenUnsentEvidence {
            cause: UnsentCause::ConnectFailed(TransportFacts::new(format!(
                "{} process-group supervision is unsupported on this platform",
                labels.process
            ))),
        });
    }
    // A cancellation already visible to the public runner outranks every
    // request-validation failure. In particular, an invalid inherited
    // environment must not turn an operation the caller already cancelled
    // into a connection failure.
    if cancellation.is_cancelled() {
        return TerminalEvidence::ProvenUnsent(ProvenUnsentEvidence {
            cause: UnsentCause::CancelledBeforeSend,
        });
    }
    if request.event_limit == 0 {
        return invalid_process_request(labels, "event limit must be nonzero");
    }
    if request.stderr_limit == 0 {
        return invalid_process_request(labels, "stderr limit must be nonzero");
    }
    if request.interrupt_grace.is_zero() {
        return invalid_process_request(labels, "interrupt grace must be nonzero");
    }
    if request
        .exchange_timeout
        .is_some_and(|timeout| timeout.is_zero())
    {
        return invalid_process_request(labels, "exchange timeout must be nonzero");
    }
    // Establish the usable deadline before command conversion, environment
    // access, SendCommenced, or spawn. `CliProcessRequest` is public and may
    // therefore carry a duration the runtime clock cannot represent even
    // though the bundled adapter constructors validate their configurations.
    let deadline = match request.exchange_timeout {
        Some(timeout) => match tokio::time::Instant::now().checked_add(timeout) {
            Some(deadline) => Some(deadline),
            None => {
                return TerminalEvidence::ProvenUnsent(ProvenUnsentEvidence {
                    cause: UnsentCause::ConnectFailed(TransportFacts::new(format!(
                        "{} exchange timeout cannot be represented by the runtime clock",
                        labels.process
                    ))),
                });
            }
        },
        None => None,
    };
    let CliProcessRequest {
        command,
        prompt,
        decoder,
        exchange_timeout: _,
        interrupt_grace,
        post_kill_reap_bound,
        event_limit,
        stderr_limit,
        environment,
        environment_overrides,
    } = request;
    let correlation = decoder.correlation().clone();
    let mut command = Command::from(command);
    command
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let environment_policy = environment;
    let environment_overrides =
        match validated_environment_overrides(environment_overrides, environment_policy) {
            Ok(overrides) => overrides,
            Err(name) => {
                return invalid_process_request(
                    labels,
                    &format!("child-environment override `{name}` is duplicate or invalid"),
                );
            }
        };
    let replaced_names = environment_overrides
        .iter()
        .filter(|value| value.replaces_inherited())
        .map(CliEnvironmentOverride::name)
        .collect::<std::collections::BTreeSet<_>>();
    let environment = match allowlisted_environment(environment_policy, labels, |name| {
        if replaced_names.contains(name) {
            None
        } else {
            std::env::var_os(name)
        }
    }) {
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
    for override_value in environment_overrides {
        let (name, value) = override_value.into_parts();
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
    if deadline.is_some_and(|deadline| tokio::time::Instant::now() >= deadline) {
        return invalid_process_request(labels, "exchange deadline elapsed before spawn");
    }

    sink.observe(Observation {
        correlation: correlation.clone(),
        fact: ObservationFact::SendCommenced,
    });
    let mut child = match command.spawn() {
        Ok(child) => SupervisedChild::new(child, post_kill_reap_bound),
        Err(error) => {
            return TerminalEvidence::ProvenUnsent(ProvenUnsentEvidence {
                cause: UnsentCause::ConnectFailed(TransportFacts::new(error.to_string())),
            });
        }
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
    match decoder.terminal_text_capture() {
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
            () = wait_for_deadline(deadline) => InputStep::TimedOut,
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
    // Survives each read future so an interrupted read can say whether it had
    // taken bytes it never delivered.
    let mut line_progress = LineProgress::AtLineBoundary;
    let mut reaped_status = None;
    let mut deadline_stderr = None;
    loop {
        let terminal_observed = decoder.terminal_observed();
        let next = tokio::select! {
            biased;
            result = read_bounded_line(&mut stdout, event_limit, labels, &mut line_progress) => ProcessStep::Line(result),
            () = &mut *cancellation => ProcessStep::Cancelled,
            () = wait_for_deadline(deadline) => ProcessStep::TimedOut,
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
                    // The failing line is already unexamined material; a suffix
                    // the same batch left buffered behind it is a second chunk
                    // nothing read, and is abandoned here too.
                    note_held_bytes(&mut decoder, line_progress);
                    return decoder.decode_failure(class, detail);
                }
                if !decoder.terminal_observed() && cancellation.is_cancelled() {
                    // Work-first: an already-exited leader's status is
                    // definitive and outranks a simultaneous cancellation.
                    // Every exit from here leaves any buffered suffix unread,
                    // whether it returns loss directly or breaks to exit
                    // classification, so the fact is recorded before both.
                    note_held_bytes(&mut decoder, line_progress);
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
                if !decoder.terminal_observed()
                    && deadline.is_some_and(|deadline| tokio::time::Instant::now() >= deadline)
                {
                    // Work-first, as in the cancellation arm: a leader that
                    // already exited on its own is definitive evidence this
                    // synchronous deadline check must not discard — a nonzero
                    // exit classifies as a provider error, and a successful
                    // exit without a terminal marker stays typed exit
                    // evidence, rather than either becoming timeout loss.
                    // As on the cancellation check above: the buffered suffix is
                    // abandoned by both the break and the direct return.
                    note_held_bytes(&mut decoder, line_progress);
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
                return read_error_loss(
                    decoder,
                    LossCause::StreamProtocolViolation {
                        detail: error.to_string(),
                    },
                    line_progress,
                );
            }
            ProcessStep::Cancelled => {
                note_held_bytes(&mut decoder, line_progress);
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
                note_held_bytes(&mut decoder, line_progress);
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
                            child.post_kill_reap_bound,
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
        () = wait_for_deadline(deadline) => {
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
                        child.post_kill_reap_bound,
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
                    () = wait_for_deadline(deadline) => {
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
                        stderr.classification().trim()
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
            let kind = D::classify_provider_error_after_exit(&classification);
            let evidence = decoder.provider_error_after_exit(&message, kind, &mut redacting_sink);
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

fn validated_environment_overrides(
    mut overrides: Vec<CliEnvironmentOverride>,
    inherited_policy: &'static [CliEnvironmentVariable],
) -> Result<Vec<CliEnvironmentOverride>, &'static str> {
    let inherited_names = inherited_policy
        .iter()
        .map(|variable| (variable.name(), *variable))
        .collect::<std::collections::BTreeMap<_, _>>();
    let mut override_names = std::collections::BTreeSet::new();
    for override_value in &mut overrides {
        let name = override_value.name();
        let inherited_conflict =
            inherited_names.contains_key(name) && !override_value.replaces_inherited();
        let missing_replacement =
            override_value.replaces_inherited() && !inherited_names.contains_key(name);
        if name.is_empty()
            || name.contains('=')
            || name.contains('\0')
            || !override_names.insert(name)
            || inherited_conflict
            || missing_replacement
        {
            return Err(name);
        }
        if override_value.replaces_inherited() {
            let Some(variable) = inherited_names.get(name) else {
                return Err(name);
            };
            let value = std::mem::take(&mut override_value.value);
            override_value.value =
                prepared_environment_value(*variable, value).map_err(|_| name)?;
        }
    }
    Ok(overrides)
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
/// proxy-userinfo rule — and a `HOME`/`CODEX_HOME` the parent cannot
/// resolve to an absolute directory, which would point the child's credential
/// store somewhere under its working directory and select an unintended ambient
/// login (see [`absolute_credential_home`]). Both must never reach the child.
fn allowlisted_environment(
    policy: &'static [CliEnvironmentVariable],
    labels: CliProcessLabels,
    read: impl Fn(&str) -> Option<std::ffi::OsString>,
) -> Result<Vec<(&'static str, std::ffi::OsString)>, EnvironmentRejection> {
    let mut environment = Vec::new();
    let mut names = std::collections::BTreeSet::new();
    for variable in policy {
        let name = variable.name();
        if !names.insert(name) {
            return Err(EnvironmentRejection {
                name,
                reason: EnvironmentRejectionReason::DuplicatePolicyName,
                labels,
            });
        }
        if let Some(value) = read(name) {
            match prepared_environment_value(*variable, value) {
                Ok(prepared) => environment.push((name, prepared)),
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
    /// One variable name appears more than once with potentially conflicting
    /// value-handling policies.
    DuplicatePolicyName,
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
            EnvironmentRejectionReason::DuplicatePolicyName => format!(
                "child-environment policy repeats `{name}`; one name cannot carry conflicting \
                 value-handling policies, so the {} exchange is refused before spawn",
                self.labels.provider
            ),
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
    variable: CliEnvironmentVariable,
    value: std::ffi::OsString,
) -> Result<std::ffi::OsString, EnvironmentRejectionReason> {
    if variable.is_credential_home() {
        return absolute_credential_home(&value, absolutize_against_current_directory)
            .ok_or(EnvironmentRejectionReason::UnresolvableCredentialHome);
    }
    match proxy_rejection(variable.name(), &value) {
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
    let post_kill_reap_bound = child.post_kill_reap_bound;
    let detail = drain_stderr_after_cleanup(
        stderr_task,
        &format!(
            "{} stderr was unavailable after cancellation",
            labels.provider
        ),
        labels,
        post_kill_reap_bound,
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
    post_kill_reap_bound: Option<Duration>,
) -> BoundedOutput {
    match optional_timeout(post_kill_reap_bound, &mut *stderr_task).await {
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

/// Whether the bounded-line reader is holding bytes it has not yet returned.
///
/// Caller-owned, because a dropped future returns nothing: this is the only way
/// to learn what an interrupted read had already taken off the transport. It is
/// equally the answer for a read that returns an error, so the reader keeps it
/// accurate on those paths rather than clearing them: a call that hands the
/// caller everything it read reports [`Self::AtLineBoundary`], and one that
/// abandons a line reports [`Self::PartialLineHeld`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineProgress {
    /// No byte of a next line has been consumed or rejected.
    AtLineBoundary,
    /// Bytes no decoder has seen are held — a line the reader consumed but has
    /// not delivered, a line it rejected at the event bound, or a suffix left
    /// buffered behind a line it did deliver. Losing them here discards them
    /// unexamined.
    PartialLineHeld,
}

/// Propagates held bytes to the decoder when the read loop is being left.
///
/// [`LineProgress::PartialLineHeld`] covers the undelivered material these exits
/// can be holding: the prefix an interrupted read had consumed, and the suffix a
/// batch left buffered behind a line the same call delivered. A line the reader
/// rejected is the third shape, and reaches [`read_error_loss`] instead, since
/// that read returned an error rather than a line. None has been examined by
/// any decoder, so every exit that can build boundary-loss evidence must record
/// the fact first — otherwise the evidence states a negative ("no tool call
/// opened") about material nothing read. The marker is sticky, so it is applied
/// only where the bytes are actually being abandoned; a loop iteration that goes
/// on to deliver the held suffix must not pre-emptively poison later evidence.
fn note_held_bytes<C, D: CliSession<C>>(decoder: &mut D, progress: LineProgress) {
    match progress {
        // Everything read has been delivered, so this exit abandons nothing and
        // the decoder's own facts still stand.
        LineProgress::AtLineBoundary => {}
        LineProgress::PartialLineHeld => decoder.note_undelivered_line(),
    }
}

/// Chooses the loss path for a failed read, from what the reader was holding.
///
/// [`read_bounded_line`] sets the boundary state before the `fill_buf` that
/// starts a line, so an error propagated from there took no byte of that line
/// and abandoned no response material: the events already decoded still answer
/// for themselves, exactly as on the cancellation and deadline arms, and this is
/// the ordinary boundary loss. Only a line the reader consumed without
/// delivering, or rejected at the event bound, is material nothing examined —
/// and only that takes the undelivered-line path, where an adapter tracking
/// per-event examination stops answering from the previous event.
fn read_error_loss<C, D: CliSession<C>>(
    decoder: D,
    cause: LossCause,
    progress: LineProgress,
) -> TerminalEvidence {
    match progress {
        LineProgress::AtLineBoundary => decoder.boundary_loss(cause),
        LineProgress::PartialLineHeld => decoder.undelivered_line_loss(cause),
    }
}

async fn read_bounded_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    limit: usize,
    labels: CliProcessLabels,
    progress: &mut LineProgress,
) -> std::io::Result<Option<Vec<u8>>> {
    *progress = LineProgress::AtLineBoundary;
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
                // Rejected, so these bytes are abandoned without any decoder
                // having seen them — the same undelivered material an
                // interrupted read holds.
                *progress = LineProgress::PartialLineHeld;
                return Err(oversize_event(limit, labels));
            }
            *progress = LineProgress::AtLineBoundary;
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
        let available_len = available.len();
        let newline = available.iter().position(|byte| *byte == b'\n');
        let take = newline.map_or(available_len, |index| index + 1);
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
            // As above: a line rejected at the bound is material nothing
            // examined, whether or not an earlier batch of it was consumed.
            *progress = LineProgress::PartialLineHeld;
            return Err(oversize_event(limit, labels));
        }
        counted += payload;
        line.extend_from_slice(&available[..take]);
        reader.consume(take);
        // Consumed and not yet delivered: from here until this call returns, an
        // interruption discards these bytes.
        if !line.is_empty() {
            *progress = LineProgress::PartialLineHeld;
        }
        if newline.is_some() {
            while matches!(line.last(), Some(b'\n' | b'\r')) {
                line.pop();
            }
            // `consume` released only this line. Anything the same batch held
            // behind it stays buffered in the reader, unseen by any decoder, so
            // the caller is still holding undelivered bytes even though this
            // call is returning a complete line.
            if take == available_len {
                *progress = LineProgress::AtLineBoundary;
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
    raw: Vec<u8>,
    classification_end: usize,
    evidence_truncated: bool,
}

impl BoundedOutput {
    fn diagnostic(text: String) -> Self {
        let raw = text.into_bytes();
        Self {
            classification_end: raw.len(),
            raw,
            evidence_truncated: false,
        }
    }

    fn classification(&self) -> String {
        let mut classification =
            String::from_utf8_lossy(&self.raw[..self.classification_end]).into_owned();
        if self.evidence_truncated {
            classification.push_str(TRUNCATION_SUFFIX);
        }
        classification
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
    Ok(BoundedOutput {
        raw: retained,
        classification_end,
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
    let text = String::from_utf8_lossy(&stderr.raw);
    let sanitized = sink.redact_terminal_failure_text(&text);
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
    if let Some(marker_start) = text[..end].rfind('[')
        && marker_start + REDACTED.len() > end
        && text[marker_start..].starts_with(REDACTED)
    {
        end = marker_start;
    }
    format!("{}{TRUNCATION_SUFFIX}", &text[..end])
}

fn invalid_process_request(labels: CliProcessLabels, reason: &str) -> TerminalEvidence {
    TerminalEvidence::ProvenUnsent(ProvenUnsentEvidence {
        cause: UnsentCause::ConnectFailed(TransportFacts::new(format!(
            "{} {reason}",
            labels.process
        ))),
    })
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
            // Apply the deployment's post-kill reap policy just as
            // `force_kill` does. A bounded policy leaves a stuck leader to its
            // drop guard; `none` explicitly waits without a deadline.
            if let Ok(Ok(_)) = optional_timeout(child.post_kill_reap_bound, child.wait()).await {
                child.disarm();
            }
            return;
        }
    }
    force_kill(child).await;
}

async fn wait_for_deadline(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

fn remaining_interrupt_grace(grace: Duration, deadline: Option<tokio::time::Instant>) -> Duration {
    deadline.map_or(grace, |deadline| {
        grace.min(deadline.saturating_duration_since(tokio::time::Instant::now()))
    })
}

async fn force_kill(child: &mut SupervisedChild) {
    kill_process_group(child.id());
    let _ = child.start_kill();
    if let Ok(Ok(_)) = optional_timeout(child.post_kill_reap_bound, child.wait()).await {
        child.disarm();
    }
}

async fn optional_timeout<F>(
    bound: Option<Duration>,
    future: F,
) -> Result<F::Output, tokio::time::error::Elapsed>
where
    F: std::future::Future,
{
    match bound {
        Some(bound) => tokio::time::timeout(bound, future).await,
        None => Ok(future.await),
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

/// Boundary loss raised before the child produced any decodable output.
///
/// The session's decoder is still owned by the caller at every one of these
/// sites and is deliberately not consulted: no provider byte has been read, so
/// it holds no view of the response's tool material and the fact is
/// [`crate::ToolCallsAtLoss::Unobserved`].
fn pre_exchange_boundary_loss(cause: LossCause) -> TerminalEvidence {
    TerminalEvidence::BoundaryLoss(crate::BoundaryLossEvidence {
        cause,
        exchange: crate::ExchangeFacts::default(),
        reported_model: None,
        finish_reported: None,
        tool_calls: crate::ToolCallsAtLoss::Unobserved,
        usage: crate::TokenUsage::unreported(),
    })
}
#[cfg(test)]
mod tests {
    use super::{
        BoundedOutput, CliDecodeFailure, CliDecodeFailureClass, CliEnvironmentOverride,
        CliEnvironmentVariable, CliProcessLabels, CliProcessRequest, CliSession,
        CliTerminalTextCapture, EnvironmentRejection, EnvironmentRejectionReason, LineProgress,
        TRUNCATION_SUFFIX, absolute_credential_home, allowlisted_environment, execute_cli_process,
        optional_timeout, read_bounded_line, read_bounded_output, read_error_loss,
        sanitized_stderr, validated_environment_overrides,
    };
    use crate::{
        BoundaryLossEvidence, CancellationSignal, ExchangeFacts, LossCause, ProviderErrorKind,
        REDACTED, RedactingSink, TerminalEvidence, TokenUsage, ToolCallsAtLoss, UnsentCause,
    };

    const TEST_ENVIRONMENT: &[CliEnvironmentVariable] = &[
        CliEnvironmentVariable::inherited("ALL_PROXY"),
        CliEnvironmentVariable::credential_home("CLAUDE_CONFIG_DIR"),
        CliEnvironmentVariable::credential_home("CODEX_HOME"),
        CliEnvironmentVariable::credential_home("HOME"),
        CliEnvironmentVariable::inherited("HTTP_PROXY"),
        CliEnvironmentVariable::inherited("HTTPS_PROXY"),
        CliEnvironmentVariable::inherited("NO_PROXY"),
        CliEnvironmentVariable::inherited("PATH"),
        CliEnvironmentVariable::inherited("TERM"),
        CliEnvironmentVariable::inherited("http_proxy"),
    ];
    const TEST_LABELS: CliProcessLabels = CliProcessLabels {
        provider: "Codex",
        process: "Codex CLI",
        decode_event: "Codex event",
        bounded_event: "Codex JSONL event",
    };

    struct UnusedSession {
        correlation: u8,
    }

    impl CliSession<u8> for UnusedSession {
        const LABELS: CliProcessLabels = TEST_LABELS;

        fn correlation(&self) -> &u8 {
            &self.correlation
        }

        fn terminal_text_capture(&self) -> CliTerminalTextCapture {
            CliTerminalTextCapture::Disabled
        }

        fn terminal_observed(&self) -> bool {
            false
        }

        fn push(
            &mut self,
            _line: &[u8],
            _sink: &mut RedactingSink<'_, u8>,
        ) -> Result<(), CliDecodeFailure> {
            Ok(())
        }

        fn decode_failure(
            self,
            _class: CliDecodeFailureClass,
            _detail: String,
        ) -> TerminalEvidence {
            unused_terminal_evidence()
        }

        fn finish(self, _sink: &mut RedactingSink<'_, u8>) -> TerminalEvidence {
            unused_terminal_evidence()
        }

        fn boundary_loss(self, _cause: LossCause) -> TerminalEvidence {
            unused_terminal_evidence()
        }

        fn boundary_loss_unless_provider_failure(
            self,
            _cause: LossCause,
            _sink: &mut RedactingSink<'_, u8>,
        ) -> TerminalEvidence {
            unused_terminal_evidence()
        }

        fn classify_provider_error_after_exit(_classification: &str) -> ProviderErrorKind {
            ProviderErrorKind::Unrecognized
        }

        fn provider_error_after_exit(
            self,
            _message: &str,
            _kind: ProviderErrorKind,
            _sink: &mut RedactingSink<'_, u8>,
        ) -> TerminalEvidence {
            unused_terminal_evidence()
        }
    }

    fn unused_terminal_evidence() -> TerminalEvidence {
        TerminalEvidence::ProvenUnsent(crate::ProvenUnsentEvidence {
            cause: UnsentCause::CancelledBeforeSend,
        })
    }

    /// A decoder whose only state is the per-event examination fact the Claude
    /// adapter tracks: its ordinary loss states the negative that fact
    /// establishes, and its undelivered-line path clears the fact first —
    /// exactly as that adapter does. The evidence therefore reports which loss
    /// path ran, which is what these tests are about.
    struct ExaminedSession {
        correlation: u8,
        examined: bool,
    }

    impl ExaminedSession {
        fn loss(self, cause: LossCause) -> TerminalEvidence {
            TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {
                cause,
                exchange: ExchangeFacts::default(),
                reported_model: None,
                finish_reported: None,
                tool_calls: if self.examined {
                    ToolCallsAtLoss::NoneOpened
                } else {
                    ToolCallsAtLoss::Unobserved
                },
                usage: TokenUsage::default(),
            })
        }
    }

    impl CliSession<u8> for ExaminedSession {
        const LABELS: CliProcessLabels = TEST_LABELS;

        fn correlation(&self) -> &u8 {
            &self.correlation
        }

        fn terminal_text_capture(&self) -> CliTerminalTextCapture {
            CliTerminalTextCapture::Disabled
        }

        fn terminal_observed(&self) -> bool {
            false
        }

        fn push(
            &mut self,
            _line: &[u8],
            _sink: &mut RedactingSink<'_, u8>,
        ) -> Result<(), CliDecodeFailure> {
            Ok(())
        }

        fn decode_failure(
            self,
            _class: CliDecodeFailureClass,
            _detail: String,
        ) -> TerminalEvidence {
            unused_terminal_evidence()
        }

        fn finish(self, _sink: &mut RedactingSink<'_, u8>) -> TerminalEvidence {
            unused_terminal_evidence()
        }

        fn boundary_loss(self, cause: LossCause) -> TerminalEvidence {
            self.loss(cause)
        }

        fn undelivered_line_loss(mut self, cause: LossCause) -> TerminalEvidence {
            self.examined = false;
            self.loss(cause)
        }

        fn boundary_loss_unless_provider_failure(
            self,
            _cause: LossCause,
            _sink: &mut RedactingSink<'_, u8>,
        ) -> TerminalEvidence {
            unused_terminal_evidence()
        }

        fn classify_provider_error_after_exit(_classification: &str) -> ProviderErrorKind {
            ProviderErrorKind::Unrecognized
        }

        fn provider_error_after_exit(
            self,
            _message: &str,
            _kind: ProviderErrorKind,
            _sink: &mut RedactingSink<'_, u8>,
        ) -> TerminalEvidence {
            unused_terminal_evidence()
        }
    }

    fn examined_session() -> ExaminedSession {
        ExaminedSession {
            correlation: 7,
            examined: true,
        }
    }

    fn read_failure_cause() -> LossCause {
        LossCause::StreamProtocolViolation {
            detail: "the process stdout read failed".to_string(),
        }
    }

    fn loss_tool_calls(evidence: TerminalEvidence) -> ToolCallsAtLoss {
        let TerminalEvidence::BoundaryLoss(loss) = evidence else {
            panic!("a failed read produces boundary loss");
        };
        loss.tool_calls
    }

    /// A reader that delivers its bytes and then fails, so a read error can be
    /// raised at a chosen point in a line.
    struct FailingReader {
        deliverable: Vec<u8>,
    }

    impl tokio::io::AsyncRead for FailingReader {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            _context: &mut std::task::Context<'_>,
            buffer: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            if self.deliverable.is_empty() {
                return std::task::Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "the process stdout read failed",
                )));
            }
            let take = self.deliverable.len().min(buffer.remaining());
            let batch: Vec<u8> = self.deliverable.drain(..take).collect();
            buffer.put_slice(&batch);
            std::task::Poll::Ready(Ok(()))
        }
    }

    fn direct_request(exchange_timeout: std::time::Duration) -> CliProcessRequest<UnusedSession> {
        CliProcessRequest {
            command: std::process::Command::new("signalbox-test-command-must-not-spawn"),
            prompt: Vec::new(),
            decoder: UnusedSession { correlation: 7 },
            exchange_timeout: Some(exchange_timeout),
            interrupt_grace: std::time::Duration::from_millis(1),
            post_kill_reap_bound: None,
            event_limit: 1_024,
            stderr_limit: 1_024,
            environment: &[],
            environment_overrides: Vec::new(),
        }
    }

    #[tokio::test]
    async fn configured_post_kill_reap_bound_times_out_pending_cleanup() {
        let outcome = optional_timeout(
            Some(std::time::Duration::ZERO),
            std::future::pending::<()>(),
        )
        .await;

        assert!(outcome.is_err());
    }

    #[tokio::test]
    async fn unbounded_post_kill_reap_waits_for_cleanup() {
        let completed_cleanup = 41_u8;
        let outcome = optional_timeout(None, std::future::ready(completed_cleanup))
            .await
            .expect("unbounded wait completes with the cleanup future");

        assert_eq!(outcome, completed_cleanup);
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
    #[tokio::test]
    async fn unrepresentable_public_deadline_is_rejected_before_send_or_spawn() {
        let mut observed = Vec::new();
        let mut cancellation = CancellationSignal::never();

        let evidence = execute_cli_process(
            direct_request(std::time::Duration::MAX),
            &mut observed,
            &mut cancellation,
        )
        .await;

        assert!(matches!(
            evidence,
            TerminalEvidence::ProvenUnsent(crate::ProvenUnsentEvidence {
                cause: UnsentCause::ConnectFailed(_),
            })
        ));
        assert!(observed.is_empty());
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
    #[tokio::test]
    async fn entry_cancellation_precedes_public_request_validation() {
        let mut observed = Vec::new();
        let mut cancellation = CancellationSignal::already_cancelled();

        let evidence = execute_cli_process(
            direct_request(std::time::Duration::MAX),
            &mut observed,
            &mut cancellation,
        )
        .await;

        assert!(matches!(
            evidence,
            TerminalEvidence::ProvenUnsent(crate::ProvenUnsentEvidence {
                cause: UnsentCause::CancelledBeforeSend,
            })
        ));
        assert!(observed.is_empty());
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
    #[tokio::test]
    async fn zero_public_event_limit_is_rejected_before_send_or_spawn() {
        let mut request = direct_request(std::time::Duration::from_secs(1));
        request.event_limit = 0;
        let mut observed = Vec::new();
        let mut cancellation = CancellationSignal::never();

        let evidence = execute_cli_process(request, &mut observed, &mut cancellation).await;

        assert!(matches!(
            evidence,
            TerminalEvidence::ProvenUnsent(crate::ProvenUnsentEvidence {
                cause: UnsentCause::ConnectFailed(_),
            })
        ));
        assert!(observed.is_empty());
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
    #[tokio::test]
    async fn zero_public_stderr_limit_is_rejected_before_send_or_spawn() {
        let mut request = direct_request(std::time::Duration::from_secs(1));
        request.stderr_limit = 0;
        let mut observed = Vec::new();
        let mut cancellation = CancellationSignal::never();

        let evidence = execute_cli_process(request, &mut observed, &mut cancellation).await;

        assert!(matches!(
            evidence,
            TerminalEvidence::ProvenUnsent(crate::ProvenUnsentEvidence {
                cause: UnsentCause::ConnectFailed(_),
            })
        ));
        assert!(observed.is_empty());
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
    #[tokio::test]
    async fn zero_public_interrupt_grace_is_rejected_before_send_or_spawn() {
        let mut request = direct_request(std::time::Duration::from_secs(1));
        request.interrupt_grace = std::time::Duration::ZERO;
        let mut observed = Vec::new();
        let mut cancellation = CancellationSignal::never();

        let evidence = execute_cli_process(request, &mut observed, &mut cancellation).await;

        assert!(matches!(
            evidence,
            TerminalEvidence::ProvenUnsent(crate::ProvenUnsentEvidence {
                cause: UnsentCause::ConnectFailed(_),
            })
        ));
        assert!(observed.is_empty());
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
    #[tokio::test]
    async fn zero_public_exchange_timeout_is_rejected_before_send_or_spawn() {
        let mut request = direct_request(std::time::Duration::ZERO);
        request.exchange_timeout = Some(std::time::Duration::ZERO);
        let mut observed = Vec::new();
        let mut cancellation = CancellationSignal::never();

        let evidence = execute_cli_process(request, &mut observed, &mut cancellation).await;

        assert!(matches!(
            evidence,
            TerminalEvidence::ProvenUnsent(crate::ProvenUnsentEvidence {
                cause: UnsentCause::ConnectFailed(_),
            })
        ));
        assert!(observed.is_empty());
    }

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
            classification_end: body.len(),
            raw: body.into_bytes(),
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
            raw: format!("api_key={SYNTHETIC_CREDENTIAL}").into_bytes(),
            classification_end: 0,
            evidence_truncated: true,
        };
        let mut observed: Vec<crate::Observation<u8>> = Vec::new();
        let sink = RedactingSink::new(&mut observed);

        let sanitized = sanitized_stderr(&sink, &stderr, usize::MAX);

        assert_eq!(sanitized, format!("api_key={REDACTED}{TRUNCATION_SUFFIX}"));
        assert!(!sanitized.contains(SYNTHETIC_CREDENTIAL));
    }

    #[test]
    fn post_sanitization_truncation_keeps_the_redaction_marker_atomic() {
        const EVIDENCE_LIMIT: usize = 12;
        let stderr = BoundedOutput {
            raw: b"api_key=x".to_vec(),
            classification_end: 0,
            evidence_truncated: false,
        };
        let mut observed: Vec<crate::Observation<u8>> = Vec::new();
        let sink = RedactingSink::new(&mut observed);

        let sanitized = sanitized_stderr(&sink, &stderr, EVIDENCE_LIMIT);

        assert_eq!(sanitized, format!("api_key={TRUNCATION_SUFFIX}"));
    }

    #[tokio::test]
    async fn stderr_reader_retains_one_extra_evidence_window_for_sanitization() {
        const EVIDENCE_LIMIT: usize = 8;
        const INPUT: &[u8] = b"abcdefghijklmnopq";

        let output = read_bounded_output(INPUT, EVIDENCE_LIMIT)
            .await
            .expect("the in-memory reader succeeds");

        assert_eq!(output.raw, &INPUT[..2 * EVIDENCE_LIMIT]);
    }

    #[tokio::test]
    async fn stderr_classification_preserves_the_original_bounded_prefix() {
        const EVIDENCE_LIMIT: usize = 8;
        const INPUT: &[u8] = b"abcdefghijklmnopq";

        let output = read_bounded_output(INPUT, EVIDENCE_LIMIT)
            .await
            .expect("the in-memory reader succeeds");

        let expected = [&INPUT[..EVIDENCE_LIMIT], TRUNCATION_SUFFIX.as_bytes()].concat();
        assert_eq!(output.classification().as_bytes(), expected);
    }

    #[tokio::test]
    async fn stderr_reader_honors_limits_above_the_response_body_default() {
        const EVIDENCE_LIMIT: usize = crate::MAX_BUFFERED_PROVIDER_RESPONSE_BYTES + 1;
        let input = vec![b'x'; EVIDENCE_LIMIT];

        let output = read_bounded_output(input.as_slice(), EVIDENCE_LIMIT)
            .await
            .expect("the in-memory reader succeeds");

        assert_eq!(output.raw, input);
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

    #[test]
    fn duplicate_environment_policy_names_are_rejected_before_assembly() {
        const DUPLICATE_POLICY: &[CliEnvironmentVariable] = &[
            CliEnvironmentVariable::credential_home("HOME"),
            CliEnvironmentVariable::inherited("HOME"),
        ];
        let repeated_name = DUPLICATE_POLICY[1].name();

        let rejection = allowlisted_environment(DUPLICATE_POLICY, TEST_LABELS, |_| None)
            .expect_err("a repeated environment-policy name must be rejected");

        assert_eq!(rejection.name, repeated_name);
        assert_eq!(
            rejection.reason,
            EnvironmentRejectionReason::DuplicatePolicyName
        );
        assert!(rejection.diagnostic().contains(repeated_name));
    }

    #[test]
    fn adapter_environment_override_cannot_replace_an_inherited_value() {
        let inherited_name = TEST_ENVIRONMENT[7].name();
        let overrides = vec![CliEnvironmentOverride::new(inherited_name, "fixture-path")];

        let rejected = validated_environment_overrides(overrides, TEST_ENVIRONMENT)
            .err()
            .expect("an ordinary override cannot replace an inherited value");

        assert_eq!(rejected, inherited_name);
    }

    #[test]
    fn explicit_replacement_is_admitted_only_for_an_inherited_name() {
        let replacement = CliEnvironmentOverride::replacing_inherited("PATH", "fixture-path");

        let admitted = validated_environment_overrides(vec![replacement], TEST_ENVIRONMENT)
            .expect("the replacement names an allowlisted inherited value");

        assert_eq!(admitted.len(), 1);
        assert_eq!(admitted[0].name(), "PATH");
    }

    #[test]
    fn explicit_replacement_rejects_a_name_outside_the_inherited_policy() {
        let replacement =
            CliEnvironmentOverride::replacing_inherited("UNDECLARED_VALUE", "fixture");

        let rejected = validated_environment_overrides(vec![replacement], TEST_ENVIRONMENT)
            .err()
            .expect("a replacement cannot widen the inherited allowlist");

        assert_eq!(rejected, "UNDECLARED_VALUE");
    }

    #[test]
    fn adapter_environment_override_cannot_replace_an_absent_declared_value() {
        let declared_name = TEST_ENVIRONMENT[3].name();
        let overrides = vec![CliEnvironmentOverride::new(
            declared_name,
            "fixture-relative",
        )];

        let rejected_name = validated_environment_overrides(overrides, TEST_ENVIRONMENT)
            .err()
            .expect("an override cannot replace an absent declared value");

        assert_eq!(rejected_name, declared_name);
    }

    #[test]
    fn distinct_adapter_environment_override_is_admitted() {
        let override_name = "FIXTURE_OUTPUT_LIMIT";
        let overrides = vec![CliEnvironmentOverride::new(override_name, "64")];

        let admitted = validated_environment_overrides(overrides, TEST_ENVIRONMENT)
            .expect("the fixed control is distinct from inherited names");

        assert_eq!(admitted[0].name(), override_name);
    }

    #[test]
    fn explicit_credential_home_replacement_is_absolutized() {
        let replacement =
            CliEnvironmentOverride::replacing_inherited("CLAUDE_CONFIG_DIR", "relative-home");

        let admitted = validated_environment_overrides(vec![replacement], TEST_ENVIRONMENT)
            .expect("credential-home replacement uses the inherited value policy");

        assert!(std::path::Path::new(&admitted[0].value).is_absolute());
    }

    #[test]
    fn explicit_proxy_replacement_rejects_embedded_userinfo() {
        let replacement = CliEnvironmentOverride::replacing_inherited(
            "HTTP_PROXY",
            "http://alice:synthetic-value@proxy.example:8080",
        );

        let rejected = validated_environment_overrides(vec![replacement], TEST_ENVIRONMENT)
            .err()
            .expect("replacement values retain the inherited proxy policy");

        assert_eq!(rejected, "HTTP_PROXY");
    }

    /// Assembles the environment with exactly `name=value` set and returns the
    /// rejection, so each test asserts the reason/name straight-line. The
    /// branching lives here in the helper, not in a test body.
    #[track_caller]
    fn rejection_for(name: &'static str, value: std::ffi::OsString) -> EnvironmentRejection {
        allowlisted_environment(TEST_ENVIRONMENT, TEST_LABELS, |queried| {
            (queried == name).then(|| value.clone())
        })
        .expect_err("the value must be rejected")
    }

    /// Assembles the environment with exactly `name=value` set and returns the
    /// assembled child value, so a passing case asserts equality straight-line.
    #[track_caller]
    fn accepted_value(name: &'static str, value: std::ffi::OsString) -> std::ffi::OsString {
        let env = allowlisted_environment(TEST_ENVIRONMENT, TEST_LABELS, |queried| {
            (queried == name).then(|| value.clone())
        })
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
        let assembled = allowlisted_environment(TEST_ENVIRONMENT, TEST_LABELS, |queried| {
            (queried == "CODEX_HOME").then(|| std::ffi::OsString::from("relative-home"))
        })
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

    /// `consume` releases only the delivered line, so a batch carrying two
    /// events leaves the second buffered in the reader where no decoder has
    /// seen it. Losing the stream there discards it.
    #[tokio::test]
    async fn a_batch_with_a_second_event_leaves_bytes_held() {
        let mut reader = std::io::Cursor::new(b"one\ntwo\n".to_vec());
        let mut progress = LineProgress::AtLineBoundary;

        let line = read_bounded_line(&mut reader, 16, TEST_LABELS, &mut progress)
            .await
            .expect("the first line is within the bound");

        assert_eq!(line.as_deref(), Some(b"one".as_slice()));
        assert_eq!(progress, LineProgress::PartialLineHeld);
    }

    /// The same read with nothing behind it leaves the caller at a boundary, so
    /// the state above is about the suffix rather than about delivering a line.
    #[tokio::test]
    async fn a_batch_with_one_event_leaves_nothing_held() {
        let mut reader = std::io::Cursor::new(b"one\n".to_vec());
        let mut progress = LineProgress::AtLineBoundary;

        let line = read_bounded_line(&mut reader, 16, TEST_LABELS, &mut progress)
            .await
            .expect("the only line is within the bound");

        assert_eq!(line.as_deref(), Some(b"one".as_slice()));
        assert_eq!(progress, LineProgress::AtLineBoundary);
    }

    #[tokio::test]
    async fn bounded_line_rejects_an_unterminated_oversize_event() {
        let input = b"12345".as_slice();
        let mut reader = tokio::io::BufReader::new(input);

        let error = read_bounded_line(
            &mut reader,
            4,
            TEST_LABELS,
            &mut LineProgress::AtLineBoundary,
        )
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

        let line = read_bounded_line(
            &mut reader,
            4,
            TEST_LABELS,
            &mut LineProgress::AtLineBoundary,
        )
        .await
        .expect("an exactly-limit payload plus its delimiter is admitted");

        assert_eq!(line.as_deref(), Some(b"1234".as_slice()));
    }

    /// The `\r` of a `\r\n` delimiter also does not count toward the limit.
    #[tokio::test]
    async fn bounded_line_admits_an_exactly_sized_payload_with_crlf() {
        let input = b"1234\r\n".as_slice();
        let mut reader = tokio::io::BufReader::new(input);

        let line = read_bounded_line(
            &mut reader,
            4,
            TEST_LABELS,
            &mut LineProgress::AtLineBoundary,
        )
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

        let line = read_bounded_line(
            &mut reader,
            4,
            TEST_LABELS,
            &mut LineProgress::AtLineBoundary,
        )
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

        let error = read_bounded_line(
            &mut reader,
            5,
            TEST_LABELS,
            &mut LineProgress::AtLineBoundary,
        )
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

        let error = read_bounded_line(
            &mut reader,
            4,
            TEST_LABELS,
            &mut LineProgress::AtLineBoundary,
        )
        .await
        .expect_err("an unterminated trailing carriage return is payload");

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    /// The same trailing carriage return within the bound is still admitted.
    #[tokio::test]
    async fn bounded_line_admits_a_deferred_carriage_return_within_the_limit() {
        let input = b"1234\r".as_slice();
        let mut reader = tokio::io::BufReader::with_capacity(5, input);

        let line = read_bounded_line(
            &mut reader,
            5,
            TEST_LABELS,
            &mut LineProgress::AtLineBoundary,
        )
        .await
        .expect("payload plus its trailing carriage return fits the bound");

        assert_eq!(line.as_deref(), Some(b"1234\r".as_slice()));
    }

    /// A payload one byte over the limit is still rejected.
    #[tokio::test]
    async fn bounded_line_rejects_an_oversize_payload_with_a_delimiter() {
        let input = b"12345\n".as_slice();
        let mut reader = tokio::io::BufReader::new(input);

        let error = read_bounded_line(
            &mut reader,
            4,
            TEST_LABELS,
            &mut LineProgress::AtLineBoundary,
        )
        .await
        .expect_err("a payload past the limit is rejected even with a delimiter");

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    /// The error `fill_buf` propagates before any byte of the next line is taken
    /// leaves the caller at a boundary, which is what lets the read-error path
    /// keep the fact the decoded prefix established.
    #[tokio::test]
    async fn a_read_error_before_the_next_line_leaves_nothing_held() {
        let mut reader = tokio::io::BufReader::new(FailingReader {
            deliverable: b"one\n".to_vec(),
        });
        let mut progress = LineProgress::AtLineBoundary;

        let line = read_bounded_line(&mut reader, 16, TEST_LABELS, &mut progress)
            .await
            .expect("the line is delivered before the transport fails");
        assert_eq!(line.as_deref(), Some(b"one".as_slice()));

        let error = read_bounded_line(&mut reader, 16, TEST_LABELS, &mut progress)
            .await
            .expect_err("the read that would start the next line fails");

        assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
        assert_eq!(progress, LineProgress::AtLineBoundary);
    }

    /// The same failure once the reader has consumed a line's opening bytes
    /// leaves them held: they came off the transport and were never delivered.
    #[tokio::test]
    async fn a_read_error_after_a_partial_line_leaves_bytes_held() {
        let mut reader = tokio::io::BufReader::new(FailingReader {
            deliverable: b"partial".to_vec(),
        });
        let mut progress = LineProgress::AtLineBoundary;

        let error = read_bounded_line(&mut reader, 16, TEST_LABELS, &mut progress)
            .await
            .expect_err("the read fails before the line is terminated");

        assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
        assert_eq!(progress, LineProgress::PartialLineHeld);
    }

    /// A line rejected at the event bound is also material no decoder examined,
    /// so the reader reports it as held rather than as a boundary — the
    /// distinction below is between abandoning a line and abandoning nothing,
    /// not between an oversize event and a transport failure.
    #[tokio::test]
    async fn a_rejected_oversize_event_leaves_bytes_held() {
        let input = b"12345\n".as_slice();
        let mut reader = tokio::io::BufReader::new(input);
        let mut progress = LineProgress::AtLineBoundary;

        read_bounded_line(&mut reader, 4, TEST_LABELS, &mut progress)
            .await
            .expect_err("a payload past the limit is rejected");

        assert_eq!(progress, LineProgress::PartialLineHeld);
    }

    /// A read that failed at a line boundary abandoned no response material, so
    /// the negative the decoded prefix established survives: an adapter that
    /// tracks per-event examination must not be told to forget it.
    #[test]
    fn a_read_error_at_a_line_boundary_keeps_the_examined_fact() {
        let evidence = read_error_loss(
            examined_session(),
            read_failure_cause(),
            LineProgress::AtLineBoundary,
        );

        assert_eq!(loss_tool_calls(evidence), ToolCallsAtLoss::NoneOpened);
    }

    /// A read that failed holding a line took bytes nothing examined, and that
    /// line could itself have carried a tool call — so the undelivered-line path
    /// still runs and the previous event stops answering for it.
    #[test]
    fn a_read_error_holding_a_line_takes_the_undelivered_line_path() {
        let evidence = read_error_loss(
            examined_session(),
            read_failure_cause(),
            LineProgress::PartialLineHeld,
        );

        assert_eq!(loss_tool_calls(evidence), ToolCallsAtLoss::Unobserved);
    }
}
