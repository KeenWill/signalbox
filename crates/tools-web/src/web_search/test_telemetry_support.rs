use signalbox_application::{
    CorrelatedToolExecutorEvidence, ToolExecutionInvocation, ToolExecutor,
};
use signalbox_domain::ToolAttemptDispatchCorrelation;
use signalbox_model_runtime::{CredentialAccessError, CredentialValue};
use std::{
    io::{self, Write},
    sync::{Arc, Mutex},
};

use super::{diagnostic::*, telemetry::*, transport_failure::*};

pub(super) const CREDENTIAL_FAILURE_CLASSIFICATION: &str = "failure=Unmapped";

pub(super) const CREDENTIAL_VALUE_FAILURE_CLASSIFICATION: &str = "failure=Unusable";

pub(super) const TRANSPORT_FAILURE_CLASSIFICATION: &str = "failure=RequestFailed";

pub(super) const RESPONSE_BODY_FAILURE_CLASSIFICATION: &str = "failure=DispatchUnknown";

pub(super) const RESPONSE_SANITIZATION_FAILURE_CLASSIFICATION: &str = "failure=EvidenceEncoding";

pub(super) const SESSION_ID_DIAGNOSTIC: &str = "session_id=00000000-0000-0000-0000-000000000001";

pub(super) const TURN_ID_DIAGNOSTIC: &str = "turn_id=00000000-0000-0000-0000-000000000002";

#[derive(Clone, Default)]
pub(super) struct CapturedTelemetry(Arc<Mutex<Vec<u8>>>);

impl CapturedTelemetry {
    pub(super) fn text(&self) -> String {
        String::from_utf8(
            self.0
                .lock()
                .expect("captured telemetry lock is available")
                .clone(),
        )
        .expect("captured telemetry is UTF-8")
    }
}

impl Write for CapturedTelemetry {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .expect("captured telemetry lock is available")
            .extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'writer> tracing_subscriber::fmt::MakeWriter<'writer> for CapturedTelemetry {
    type Writer = Self;

    fn make_writer(&'writer self) -> Self::Writer {
        self.clone()
    }
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
    let output = CapturedTelemetry::default();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(output.clone())
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        report_credential_access_failure(error, correlation);
    });
    output.text()
}

pub(super) fn capture_credential_failure_in_credential_span(
    error: &CredentialAccessError,
    correlation: &ToolAttemptDispatchCorrelation,
    credential: &str,
) -> String {
    let output = CapturedTelemetry::default();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(output.clone())
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        let caller = tracing::warn_span!("caller", credential);
        let _entered = caller.enter();
        report_credential_access_failure(error, correlation);
    });
    output.text()
}

pub(super) fn capture_credential_value_failure(
    correlation: &ToolAttemptDispatchCorrelation,
    credential: &CredentialValue,
) -> (String, Result<(), WebSearchExecutorError>) {
    let output = CapturedTelemetry::default();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(output.clone())
        .finish();
    let result = tracing::subscriber::with_default(subscriber, || {
        report_credential_value_failure(correlation, credential)
    });
    (output.text(), result)
}

pub(super) fn fully_percent_encode(value: &str) -> String {
    value.bytes().map(|byte| format!("%{byte:02X}")).collect()
}

pub(super) fn capture_credential_value_failure_in_credential_span(
    correlation: &ToolAttemptDispatchCorrelation,
    credential: &CredentialValue,
) -> (String, Result<(), WebSearchExecutorError>) {
    let output = CapturedTelemetry::default();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(output.clone())
        .finish();
    let credential_text =
        std::str::from_utf8(credential.expose_bytes()).expect("fixture credential is UTF-8");
    let result = tracing::subscriber::with_default(subscriber, || {
        let caller = tracing::warn_span!("caller", credential = credential_text);
        let _entered = caller.enter();
        report_credential_value_failure(correlation, credential)
    });
    (output.text(), result)
}

pub(super) fn capture_transport_failure(
    failure: &WebSearchTransportFailure,
    correlation: &ToolAttemptDispatchCorrelation,
    credential: &CredentialValue,
) -> (String, Result<(), WebSearchExecutorError>) {
    let output = CapturedTelemetry::default();
    let subscriber = tracing_subscriber::fmt()
        .compact()
        .with_writer(output.clone())
        .finish();
    let result = tracing::subscriber::with_default(subscriber, || {
        report_transport_failure(failure, correlation, credential)
    });
    (output.text(), result)
}

pub(super) fn capture_response_body_failure(
    failure_class: WebSearchTransportFailureClass,
    correlation: &ToolAttemptDispatchCorrelation,
    credential: &CredentialValue,
) -> (String, Result<(), WebSearchExecutorError>) {
    let output = CapturedTelemetry::default();
    let subscriber = tracing_subscriber::fmt()
        .compact()
        .with_writer(output.clone())
        .finish();
    let result = tracing::subscriber::with_default(subscriber, || {
        report_response_body_failure(failure_class, correlation, credential)
    });
    (output.text(), result)
}

pub(super) fn capture_response_sanitization_failure(
    correlation: &ToolAttemptDispatchCorrelation,
    credential: &CredentialValue,
) -> (String, Result<(), WebSearchExecutorError>) {
    let output = CapturedTelemetry::default();
    let subscriber = tracing_subscriber::fmt()
        .compact()
        .with_writer(output.clone())
        .finish();
    let result = tracing::subscriber::with_default(subscriber, || {
        report_response_sanitization_failure(correlation, credential)
    });
    (output.text(), result)
}
