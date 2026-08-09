use signalbox_application::{
    CorrelatedToolExecutorEvidence, ToolExecutionInvocation, ToolExecutor,
};
use signalbox_domain::ToolAttemptDispatchCorrelation;
use signalbox_model_runtime::{CredentialAccessError, CredentialValue};
use std::{
    cell::RefCell,
    io::{self, Write},
    sync::{Arc, Mutex, OnceLock},
};

use super::{diagnostic::*, telemetry::*, transport_failure::*};

pub(super) const CREDENTIAL_FAILURE_CLASSIFICATION: &str = "failure=Unmapped";

pub(super) const CREDENTIAL_VALUE_FAILURE_CLASSIFICATION: &str = "failure=Unusable";

pub(super) const TRANSPORT_FAILURE_CLASSIFICATION: &str = "failure=RequestFailed";

pub(super) const RESPONSE_BODY_FAILURE_CLASSIFICATION: &str = "failure=DispatchUnknown";

pub(super) const RESPONSE_SANITIZATION_FAILURE_CLASSIFICATION: &str = "failure=EvidenceEncoding";

pub(super) const SESSION_ID_DIAGNOSTIC: &str = "session_id=00000000-0000-0000-0000-000000000001";

pub(super) const TURN_ID_DIAGNOSTIC: &str = "turn_id=00000000-0000-0000-0000-000000000002";

thread_local! {
    /// Telemetry captured on this thread alone.
    static CAPTURED_TELEMETRY: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

/// Appends every formatted event to the emitting thread's own buffer.
#[derive(Clone, Copy, Default)]
pub(super) struct CapturedTelemetry;

impl Write for CapturedTelemetry {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        CAPTURED_TELEMETRY.with(|captured| captured.borrow_mut().extend_from_slice(buffer));
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'writer> tracing_subscriber::fmt::MakeWriter<'writer> for CapturedTelemetry {
    type Writer = Self;

    fn make_writer(&'writer self) -> Self::Writer {
        *self
    }
}

/// Installs the capturing subscriber once for the whole test process, and
/// clears whatever this thread captured earlier.
///
/// It must be global rather than thread-scoped. `tracing` caches each
/// callsite's interest process-wide, but `set_default` binds a subscriber to
/// one thread, so a sibling test that reaches a callsite first on another
/// thread registers it against no subscriber at all -- recording it as
/// uninteresting for every thread, including the one that installed a capture.
/// The event then is not merely written late; it is never emitted, and the
/// assertion reads an empty buffer.
///
/// Writes are routed per thread so concurrent tests never read each other's
/// events, which keeps assertions on both presence and absence honest.
pub(super) fn capture_telemetry_for_this_thread() {
    static INSTALLED: OnceLock<()> = OnceLock::new();

    INSTALLED.get_or_init(|| {
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(CapturedTelemetry)
            .finish();
        tracing::subscriber::set_global_default(subscriber)
            .expect("no other global telemetry subscriber is installed");
    });
    CAPTURED_TELEMETRY.with(|captured| captured.borrow_mut().clear());
}

/// Returns the telemetry captured on this thread.
pub(super) fn captured_telemetry() -> String {
    CAPTURED_TELEMETRY
        .with(|captured| String::from_utf8(captured.borrow().clone()))
        .expect("captured telemetry is UTF-8")
}

pub(super) struct FormattingExecutor<Executor> {
    pub(super) inner: Executor,
    pub(super) diagnostic: Arc<Mutex<String>>,
}

impl<Executor> ToolExecutor for FormattingExecutor<Executor>
where
    Executor: ToolExecutor<Error = WebSearchExecutorError> + Send,
{
    type Error = WebSearchExecutorError;

    async fn execute(
        &mut self,
        invocation: ToolExecutionInvocation,
    ) -> Result<CorrelatedToolExecutorEvidence, Self::Error> {
        let result = self.inner.execute(invocation).await;
        *self
            .diagnostic
            .lock()
            .expect("captured executor diagnostic lock is available") = format!("{result:?}");
        result
    }
}

pub(super) fn capture_credential_failure(
    error: &CredentialAccessError,
    correlation: &ToolAttemptDispatchCorrelation,
) -> String {
    capture_telemetry_for_this_thread();
    report_credential_access_failure(error, correlation);
    captured_telemetry()
}

pub(super) fn capture_credential_failure_in_credential_span(
    error: &CredentialAccessError,
    correlation: &ToolAttemptDispatchCorrelation,
    credential: &str,
) -> String {
    capture_telemetry_for_this_thread();
    let caller = tracing::warn_span!("caller", credential);
    let _entered = caller.enter();
    report_credential_access_failure(error, correlation);
    captured_telemetry()
}

pub(super) fn capture_credential_value_failure(
    correlation: &ToolAttemptDispatchCorrelation,
    credential: &CredentialValue,
) -> (String, Result<(), WebSearchExecutorError>) {
    capture_telemetry_for_this_thread();
    let result = report_credential_value_failure(correlation, credential);
    (captured_telemetry(), result)
}

pub(super) fn fully_percent_encode(value: &str) -> String {
    value.bytes().map(|byte| format!("%{byte:02X}")).collect()
}

pub(super) fn capture_credential_value_failure_in_credential_span(
    correlation: &ToolAttemptDispatchCorrelation,
    credential: &CredentialValue,
) -> (String, Result<(), WebSearchExecutorError>) {
    capture_telemetry_for_this_thread();
    let credential_text =
        std::str::from_utf8(credential.expose_bytes()).expect("fixture credential is UTF-8");
    let caller = tracing::warn_span!("caller", credential = credential_text);
    let _entered = caller.enter();
    let result = report_credential_value_failure(correlation, credential);
    (captured_telemetry(), result)
}

pub(super) fn capture_transport_failure(
    failure: &WebSearchTransportFailure,
    correlation: &ToolAttemptDispatchCorrelation,
    credential: &CredentialValue,
) -> (String, Result<(), WebSearchExecutorError>) {
    capture_telemetry_for_this_thread();
    let result = report_transport_failure(failure, correlation, credential);
    (captured_telemetry(), result)
}

pub(super) fn capture_response_body_failure(
    failure_class: WebSearchTransportFailureClass,
    correlation: &ToolAttemptDispatchCorrelation,
    credential: &CredentialValue,
) -> (String, Result<(), WebSearchExecutorError>) {
    capture_telemetry_for_this_thread();
    let result = report_response_body_failure(failure_class, correlation, credential);
    (captured_telemetry(), result)
}

pub(super) fn capture_response_sanitization_failure(
    correlation: &ToolAttemptDispatchCorrelation,
    credential: &CredentialValue,
) -> (String, Result<(), WebSearchExecutorError>) {
    capture_telemetry_for_this_thread();
    let result = report_response_sanitization_failure(correlation, credential);
    (captured_telemetry(), result)
}
