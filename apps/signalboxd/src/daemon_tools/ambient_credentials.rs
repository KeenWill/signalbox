use std::io::Write;

use signalbox_application::{
    CorrelatedToolExecutorEvidence, ToolExecutionInvocation, ToolExecutorEvidence,
};
use signalbox_domain::ToolExecutionErrorDetail;
use signalbox_model_runtime::redact_credential_text;
use signalbox_tools_exec::{
    ExecExecutor, ProcessRunner, SandboxConfiguration, SandboxReadOnlyMount,
    SandboxedCommandRunner, SandboxedExecArguments,
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
        Ok(result) => match serde_json::to_string(&result) {
            Ok(result) => invocation.bind(ToolExecutorEvidence::CompletedText(
                redact_credential_text(result, &credential),
            )),
            Err(_) => failed(invocation, "ambient task result unavailable"),
        },
        Err(_) => failed(invocation, "ambient task arguments invalid"),
    }
}

fn failed(invocation: ToolExecutionInvocation, detail: &str) -> CorrelatedToolExecutorEvidence {
    invocation.bind(ToolExecutorEvidence::KnownFailed {
        detail: ToolExecutionErrorDetail::try_new(detail.to_owned()).ok(),
    })
}
