use std::io::Write;

use signalbox_application::{
    CorrelatedToolExecutorEvidence, ToolExecutionInvocation, ToolExecutorEvidence,
};
use signalbox_domain::ToolExecutionErrorDetail;
use signalbox_model_runtime::{CredentialValue, redact_credential_text};
use signalbox_tools_exec::{
    CaptureCompleteness, ExecExecutor, OutputCapture, OutputEncoding, ProcessRunner,
    SandboxConfiguration, SandboxReadOnlyMount, SandboxedCommandRunner, SandboxedExecArguments,
};

use crate::{configuration_reload::ConfigurationReload, credential_pools::AmbientCredentialSource};

pub(super) async fn execute<Runner: ProcessRunner>(
    catalogs: &ConfigurationReload,
    pool: &sqlx::PgPool,
    executor: &mut ExecExecutor<SandboxedCommandRunner<Runner>>,
    invocation: ToolExecutionInvocation,
    base: &SandboxConfiguration,
    arguments: SandboxedExecArguments,
) -> CorrelatedToolExecutorEvidence {
    let Some(purpose) = arguments.credential_purpose else {
        return failed(invocation, "ambient credential purpose is missing");
    };
    let approved = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM tool_approval_decision
          WHERE request_id = $1 AND decision_kind = 'approve' AND decision_source = 'delegate')",
    )
    .bind(invocation.request().id().into_uuid())
    .fetch_one(pool)
    .await;
    if !matches!(approved, Ok(true)) {
        return failed(
            invocation,
            "ambient credential requires approval of this request by the judge",
        );
    }
    let models = catalogs.catalogs().models;
    let (source, credential) = match models.resolve_ambient_task_credential(&purpose).await {
        Ok(resolved) => resolved,
        Err(_) => return failed(invocation, "ambient credential unavailable"),
    };
    let mut sandbox = base.clone();
    let _file = match source {
        AmbientCredentialSource::File(destination) => {
            let mut file = match tempfile::NamedTempFile::new() {
                Ok(file) => file,
                Err(_) => return failed(invocation, "ambient credential unavailable"),
            };
            if file.write_all(credential.expose_bytes()).is_err() {
                return failed(invocation, "ambient credential unavailable");
            }
            sandbox.read_only_mounts.push(SandboxReadOnlyMount {
                source: file.path().to_owned(),
                destination,
            });
            Some(file)
        }
        AmbientCredentialSource::Environment(variable) => {
            use std::os::unix::ffi::OsStringExt;
            sandbox.environment.insert(
                variable.as_ref().into(),
                std::ffi::OsString::from_vec(credential.expose_bytes().to_vec()),
            );
            None
        }
    };
    match executor
        .run_with_configuration(arguments.command, sandbox)
        .await
    {
        Ok(mut result) => {
            redact_truncated_credential_prefix(&mut result.stdout, &credential);
            redact_truncated_credential_prefix(&mut result.stderr, &credential);
            match serde_json::to_string(&result) {
                Ok(result) => invocation.bind(ToolExecutorEvidence::CompletedText(
                    redact_credential_text(result, &credential),
                )),
                Err(_) => failed(invocation, "ambient task result unavailable"),
            }
        }
        Err(_) => failed(invocation, "ambient task arguments invalid"),
    }
}

fn redact_truncated_credential_prefix(capture: &mut OutputCapture, credential: &CredentialValue) {
    if capture.completeness != CaptureCompleteness::Truncated {
        return;
    }
    let Ok(secret) = std::str::from_utf8(credential.expose_bytes()) else {
        capture.text = "[redacted]".to_owned();
        return;
    };
    if secret.is_empty() {
        return;
    }
    if capture.encoding == OutputEncoding::LossyUtf8 {
        capture.text = "[redacted]".to_owned();
        return;
    }
    let Some(prefix_bytes) = (1..=secret.len()).rev().find(|length| {
        secret.is_char_boundary(*length) && capture.text.ends_with(&secret[..*length])
    }) else {
        return;
    };
    capture
        .text
        .truncate(capture.text.len().saturating_sub(prefix_bytes));
    capture.text.push_str("[redacted]");
}

fn failed(invocation: ToolExecutionInvocation, detail: &str) -> CorrelatedToolExecutorEvidence {
    invocation.bind(ToolExecutorEvidence::KnownFailed {
        detail: ToolExecutionErrorDetail::try_new(detail.to_owned()).ok(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncated_credential_prefix_is_redacted() {
        let credential = CredentialValue::new(b"secret-prefix".to_vec());
        let mut capture = OutputCapture {
            text: "padding-secret-pre".to_owned(),
            completeness: CaptureCompleteness::Truncated,
            encoding: OutputEncoding::Utf8,
        };

        redact_truncated_credential_prefix(&mut capture, &credential);

        assert_eq!(capture.text, "padding-[redacted]");
    }

    #[test]
    fn truncated_lossy_capture_is_redacted_fail_closed() {
        let credential = CredentialValue::new("secret-☃suffix".as_bytes().to_vec());
        let mut capture = OutputCapture {
            text: "padding-secret-�".to_owned(),
            completeness: CaptureCompleteness::Truncated,
            encoding: OutputEncoding::LossyUtf8,
        };

        redact_truncated_credential_prefix(&mut capture, &credential);

        assert_eq!(capture.text, "[redacted]");
    }

    #[test]
    fn complete_capture_is_left_for_full_value_redaction() {
        let credential = CredentialValue::new(b"secret-prefix".to_vec());
        let mut capture = OutputCapture {
            text: "padding-secret-pre".to_owned(),
            completeness: CaptureCompleteness::Complete,
            encoding: OutputEncoding::Utf8,
        };

        redact_truncated_credential_prefix(&mut capture, &credential);

        assert_eq!(capture.text, "padding-secret-pre");
    }
}
