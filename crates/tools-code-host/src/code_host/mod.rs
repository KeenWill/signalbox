//! Credentialed daemon-side code-host review tools.

mod arguments;
mod change_request_changed_files;
mod change_request_checks_status;
mod change_request_ci_job_log;
mod change_request_comment;
mod change_request_file_patch;
mod change_request_rerun_failed_jobs;
mod change_request_review_threads;
mod change_request_summary;
mod change_request_thread_reply;
mod change_request_thread_resolve;
mod github;
mod result;

use std::{error::Error, fmt, future::Future};

use signalbox_application::{
    ClassifyOperatorFailure, CompiledTool, CompiledToolCatalog, CorrelatedToolExecutorEvidence,
    OperatorFailureClass, ToolArgumentValidator, ToolDefinition, ToolExecutionInvocation,
    ToolExecutor, ToolExecutorEvidence,
};
use signalbox_domain::{
    NormalizedToolArguments, ToolEffectClass, ToolExecutionErrorDetail, ToolPermissionDefault,
};
use signalbox_model_runtime::{CredentialAccess, CredentialReference, CredentialValue};
use signalbox_tool_contract::{ToolContractCompileError, compile_contract_definition};

pub use arguments::{
    CodeHostChangeRequestNumber, CodeHostCommentBody, CodeHostFilePath, CodeHostOpaqueId,
    CodeHostRepository, CodeHostRevision,
};
pub use change_request_changed_files::ChangedFilesArguments;
pub use change_request_checks_status::ChecksStatusArguments;
pub use change_request_ci_job_log::CiJobLogArguments;
pub use change_request_comment::ChangeRequestCommentArguments;
pub use change_request_file_patch::FilePatchArguments;
pub use change_request_rerun_failed_jobs::RerunFailedJobsArguments;
pub use change_request_review_threads::ReviewThreadsArguments;
pub use change_request_summary::ChangeRequestSummaryArguments;
pub use change_request_thread_reply::ThreadReplyArguments;
pub use change_request_thread_resolve::ThreadResolveArguments;
pub use github::{GitHubCodeHostConstructionError, GitHubCodeHostTransport};
pub use result::{
    ChangeRequestCommentResult, ChangeRequestSummaryFields, ChangeRequestSummaryResult,
    ChangedFile, ChangedFilesResult, CheckStatus, ChecksStatusResult, CiJobLogResult,
    CodeHostResult, CodeHostResultCompleteness, CodeHostUrl, FilePatchResult,
    RerunFailedJobsResult, ReviewThread, ReviewThreadComment, ReviewThreadFields,
    ReviewThreadResolution, ReviewThreadsResult, ThreadReplyResult, ThreadResolveResult,
};

/// Non-secret name of the daemon-held code-host credential.
pub const CODE_HOST_CREDENTIAL_REFERENCE: &str = "github-primary";

/// Registry name for change-request summary lookup.
pub const CHANGE_REQUEST_SUMMARY_NAME: &str = change_request_summary::NAME;
/// Registry name for changed-files lookup.
pub const CHANGE_REQUEST_CHANGED_FILES_NAME: &str = change_request_changed_files::NAME;
/// Registry name for per-file patch lookup.
pub const CHANGE_REQUEST_FILE_PATCH_NAME: &str = change_request_file_patch::NAME;
/// Registry name for check-status lookup.
pub const CHANGE_REQUEST_CHECKS_STATUS_NAME: &str = change_request_checks_status::NAME;
/// Registry name for top-level comment creation.
pub const CHANGE_REQUEST_COMMENT_NAME: &str = change_request_comment::NAME;
/// Registry name for review-thread lookup.
pub const CHANGE_REQUEST_REVIEW_THREADS_NAME: &str = change_request_review_threads::NAME;
/// Registry name for review-thread replies.
pub const CHANGE_REQUEST_THREAD_REPLY_NAME: &str = change_request_thread_reply::NAME;
/// Registry name for review-thread resolution.
pub const CHANGE_REQUEST_THREAD_RESOLVE_NAME: &str = change_request_thread_resolve::NAME;
/// Registry name for CI job-log lookup.
pub const CHANGE_REQUEST_CI_JOB_LOG_NAME: &str = change_request_ci_job_log::NAME;
/// Registry name for failed-job reruns.
pub const CHANGE_REQUEST_RERUN_FAILED_JOBS_NAME: &str = change_request_rerun_failed_jobs::NAME;

/// Registry names in deterministic catalog order.
pub const CODE_HOST_TOOL_NAMES: [&str; 10] = [
    CHANGE_REQUEST_CHANGED_FILES_NAME,
    CHANGE_REQUEST_CHECKS_STATUS_NAME,
    CHANGE_REQUEST_CI_JOB_LOG_NAME,
    CHANGE_REQUEST_COMMENT_NAME,
    CHANGE_REQUEST_FILE_PATCH_NAME,
    CHANGE_REQUEST_RERUN_FAILED_JOBS_NAME,
    CHANGE_REQUEST_REVIEW_THREADS_NAME,
    CHANGE_REQUEST_SUMMARY_NAME,
    CHANGE_REQUEST_THREAD_REPLY_NAME,
    CHANGE_REQUEST_THREAD_RESOLVE_NAME,
];

const INVALID_ARGUMENTS_DETAIL: &str = "code-host tool arguments are invalid";
const CREDENTIAL_UNAVAILABLE_DETAIL: &str = "code-host credential is unavailable";
const CODE_HOST_REJECTED_DETAIL: &str =
    "code host rejected the request or returned an invalid bounded response";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CodeHostToolKind {
    /// Registry effect posture: read-only, `Auto`, and `ExternalEffect`
    /// because the code host observes the authenticated request.
    Summary,
    /// Registry effect posture: read-only, `Auto`, and `ExternalEffect`.
    ChangedFiles,
    /// Registry effect posture: read-only, `Auto`, and `ExternalEffect`.
    FilePatch,
    /// Registry effect posture: read-only, `Auto`, and `ExternalEffect`.
    ChecksStatus,
    /// Registry effect posture: mutation, `Confirm`, and `ExternalEffect`.
    Comment,
    /// Registry effect posture: read-only, `Auto`, and `ExternalEffect`.
    ReviewThreads,
    /// Registry effect posture: mutation, `Confirm`, and `ExternalEffect`.
    ThreadReply,
    /// Registry effect posture: mutation, `Confirm`, and `ExternalEffect`.
    ThreadResolve,
    /// Registry effect posture: read-only, `Auto`, and `ExternalEffect`.
    CiJobLog,
    /// Registry effect posture: mutation, `Confirm`, and `ExternalEffect`.
    RerunFailedJobs,
}

impl CodeHostToolKind {
    const ALL: [Self; 10] = [
        Self::Summary,
        Self::ChangedFiles,
        Self::FilePatch,
        Self::ChecksStatus,
        Self::Comment,
        Self::ReviewThreads,
        Self::ThreadReply,
        Self::ThreadResolve,
        Self::CiJobLog,
        Self::RerunFailedJobs,
    ];

    const fn name(self) -> &'static str {
        match self {
            Self::Summary => change_request_summary::NAME,
            Self::ChangedFiles => change_request_changed_files::NAME,
            Self::FilePatch => change_request_file_patch::NAME,
            Self::ChecksStatus => change_request_checks_status::NAME,
            Self::Comment => change_request_comment::NAME,
            Self::ReviewThreads => change_request_review_threads::NAME,
            Self::ThreadReply => change_request_thread_reply::NAME,
            Self::ThreadResolve => change_request_thread_resolve::NAME,
            Self::CiJobLog => change_request_ci_job_log::NAME,
            Self::RerunFailedJobs => change_request_rerun_failed_jobs::NAME,
        }
    }

    fn definition(self) -> Result<ToolDefinition, ToolContractCompileError> {
        let permission = self.permission();
        match self {
            Self::Summary => compile_contract_definition::<change_request_summary::Contract>(
                permission,
                ToolEffectClass::ExternalEffect,
            ),
            Self::ChangedFiles => compile_contract_definition::<
                change_request_changed_files::Contract,
            >(permission, ToolEffectClass::ExternalEffect),
            Self::FilePatch => compile_contract_definition::<change_request_file_patch::Contract>(
                permission,
                ToolEffectClass::ExternalEffect,
            ),
            Self::ChecksStatus => compile_contract_definition::<
                change_request_checks_status::Contract,
            >(permission, ToolEffectClass::ExternalEffect),
            Self::Comment => compile_contract_definition::<change_request_comment::Contract>(
                permission,
                ToolEffectClass::ExternalEffect,
            ),
            Self::ReviewThreads => compile_contract_definition::<
                change_request_review_threads::Contract,
            >(permission, ToolEffectClass::ExternalEffect),
            Self::ThreadReply => {
                compile_contract_definition::<change_request_thread_reply::Contract>(
                    permission,
                    ToolEffectClass::ExternalEffect,
                )
            }
            Self::ThreadResolve => compile_contract_definition::<
                change_request_thread_resolve::Contract,
            >(permission, ToolEffectClass::ExternalEffect),
            Self::CiJobLog => compile_contract_definition::<change_request_ci_job_log::Contract>(
                permission,
                ToolEffectClass::ExternalEffect,
            ),
            Self::RerunFailedJobs => compile_contract_definition::<
                change_request_rerun_failed_jobs::Contract,
            >(permission, ToolEffectClass::ExternalEffect),
        }
    }

    const fn permission(self) -> ToolPermissionDefault {
        match self {
            Self::Comment | Self::ThreadReply | Self::ThreadResolve | Self::RerunFailedJobs => {
                ToolPermissionDefault::Confirm
            }
            Self::Summary
            | Self::ChangedFiles
            | Self::FilePatch
            | Self::ChecksStatus
            | Self::ReviewThreads
            | Self::CiJobLog => ToolPermissionDefault::Auto,
        }
    }

    const fn mutates(self) -> bool {
        matches!(
            self,
            Self::Comment | Self::ThreadReply | Self::ThreadResolve | Self::RerunFailedJobs
        )
    }

    fn accepts_result(self, result: &CodeHostResult) -> bool {
        matches!(
            (self, result),
            (Self::Summary, CodeHostResult::Summary(_))
                | (Self::ChangedFiles, CodeHostResult::ChangedFiles(_))
                | (Self::FilePatch, CodeHostResult::FilePatch(_))
                | (Self::ChecksStatus, CodeHostResult::ChecksStatus(_))
                | (Self::Comment, CodeHostResult::Comment(_))
                | (Self::ReviewThreads, CodeHostResult::ReviewThreads(_))
                | (Self::ThreadReply, CodeHostResult::ThreadReply(_))
                | (Self::ThreadResolve, CodeHostResult::ThreadResolve(_))
                | (Self::CiJobLog, CodeHostResult::CiJobLog(_))
                | (Self::RerunFailedJobs, CodeHostResult::RerunFailedJobs(_))
        )
    }
}

fn kind_for_name(name: &str) -> Option<CodeHostToolKind> {
    CodeHostToolKind::ALL
        .into_iter()
        .find(|kind| kind.name() == name)
}

/// Closed typed operation vocabulary crossing the mocked code-host transport.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CodeHostOperation {
    /// Read one change-request summary.
    Summary(ChangeRequestSummaryArguments),
    /// Read one changed-file page.
    ChangedFiles(ChangedFilesArguments),
    /// Read one file patch.
    FilePatch(FilePatchArguments),
    /// Read checks for one frozen revision.
    ChecksStatus(ChecksStatusArguments),
    /// Post one top-level comment.
    Comment(ChangeRequestCommentArguments),
    /// Read one review-thread page.
    ReviewThreads(ReviewThreadsArguments),
    /// Reply to one review thread.
    ThreadReply(ThreadReplyArguments),
    /// Resolve one review thread.
    ThreadResolve(ThreadResolveArguments),
    /// Read one CI job log.
    CiJobLog(CiJobLogArguments),
    /// Rerun failed jobs in one workflow run.
    RerunFailedJobs(RerunFailedJobsArguments),
}

impl CodeHostOperation {
    /// Returns the registry tool name that produced this operation.
    pub const fn tool_name(&self) -> &'static str {
        match self {
            Self::Summary(_) => change_request_summary::NAME,
            Self::ChangedFiles(_) => change_request_changed_files::NAME,
            Self::FilePatch(_) => change_request_file_patch::NAME,
            Self::ChecksStatus(_) => change_request_checks_status::NAME,
            Self::Comment(_) => change_request_comment::NAME,
            Self::ReviewThreads(_) => change_request_review_threads::NAME,
            Self::ThreadReply(_) => change_request_thread_reply::NAME,
            Self::ThreadResolve(_) => change_request_thread_resolve::NAME,
            Self::CiJobLog(_) => change_request_ci_job_log::NAME,
            Self::RerunFailedJobs(_) => change_request_rerun_failed_jobs::NAME,
        }
    }
}

fn decode_operation(
    kind: CodeHostToolKind,
    arguments: &NormalizedToolArguments,
) -> Result<CodeHostOperation, arguments::InvalidCodeHostArguments> {
    match kind {
        CodeHostToolKind::Summary => change_request_summary::decode(arguments),
        CodeHostToolKind::ChangedFiles => change_request_changed_files::decode(arguments),
        CodeHostToolKind::FilePatch => change_request_file_patch::decode(arguments),
        CodeHostToolKind::ChecksStatus => change_request_checks_status::decode(arguments),
        CodeHostToolKind::Comment => change_request_comment::decode(arguments),
        CodeHostToolKind::ReviewThreads => change_request_review_threads::decode(arguments),
        CodeHostToolKind::ThreadReply => change_request_thread_reply::decode(arguments),
        CodeHostToolKind::ThreadResolve => change_request_thread_resolve::decode(arguments),
        CodeHostToolKind::CiJobLog => change_request_ci_job_log::decode(arguments),
        CodeHostToolKind::RerunFailedJobs => change_request_rerun_failed_jobs::decode(arguments),
    }
}

/// Sanitized physical code-host failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CodeHostTransportFailure {
    /// Credential bytes could not form the authentication header.
    InvalidCredential,
    /// A definitive response rejected the operation.
    Rejected,
    /// The bounded response did not match the typed result contract.
    InvalidResponse,
    /// Response bytes exceeded the fixed operation cap.
    ResponseTooLarge,
    /// Transport loss left physical dispatch unknown.
    DispatchUnknown,
}

/// Mockable typed transport boundary for the code-host suite.
pub trait CodeHostTransport: Send {
    /// Executes the exact typed operation with one request-scoped credential.
    fn execute(
        &mut self,
        operation: CodeHostOperation,
        credential: &CredentialValue,
    ) -> impl Future<Output = Result<CodeHostResult, CodeHostTransportFailure>> + Send;
}

#[derive(Clone, Debug)]
struct CodeHostArgumentValidator {
    kind: CodeHostToolKind,
    detail: ToolExecutionErrorDetail,
}

impl ToolArgumentValidator for CodeHostArgumentValidator {
    fn validate(
        &self,
        arguments: &NormalizedToolArguments,
    ) -> Result<(), ToolExecutionErrorDetail> {
        decode_operation(self.kind, arguments)
            .map(|_| ())
            .map_err(|_| self.detail.clone())
    }
}

/// All ten compiled code-host declarations and their matching executor.
///
/// Effect postures are explicit per declaration: summary, files, patch,
/// checks, threads, and job-log reads are `Auto`; comment, reply, resolve, and
/// rerun mutations are `Confirm`. Every operation is `ExternalEffect` because
/// even an authenticated read is visible to the remote code host.
#[derive(Clone, Debug)]
pub struct CodeHostTools<Credentials, Transport> {
    catalog: CompiledToolCatalog,
    executor: CodeHostExecutor<Credentials, Transport>,
}

impl<Credentials, Transport> CodeHostTools<Credentials, Transport> {
    /// Compiles the fixed declarations around injected credential and transport
    /// boundaries.
    pub fn try_new(
        credentials: Credentials,
        transport: Transport,
    ) -> Result<Self, CodeHostToolsConstructionError> {
        let invalid_arguments_detail =
            ToolExecutionErrorDetail::try_new(String::from(INVALID_ARGUMENTS_DETAIL))
                .map_err(|_| CodeHostToolsConstructionError::ErrorDetail)?;
        let credential_unavailable_detail =
            ToolExecutionErrorDetail::try_new(String::from(CREDENTIAL_UNAVAILABLE_DETAIL))
                .map_err(|_| CodeHostToolsConstructionError::ErrorDetail)?;
        let code_host_rejected_detail =
            ToolExecutionErrorDetail::try_new(String::from(CODE_HOST_REJECTED_DETAIL))
                .map_err(|_| CodeHostToolsConstructionError::ErrorDetail)?;
        let mut compiled = Vec::with_capacity(CodeHostToolKind::ALL.len());
        for kind in CodeHostToolKind::ALL {
            let definition = kind.definition().map_err(|error| match error {
                ToolContractCompileError::Name => CodeHostToolsConstructionError::Name,
                ToolContractCompileError::Schema => CodeHostToolsConstructionError::Schema,
            })?;
            compiled.push(CompiledTool::new(
                definition,
                CodeHostArgumentValidator {
                    kind,
                    detail: invalid_arguments_detail.clone(),
                },
            ));
        }
        let catalog = CompiledToolCatalog::try_new(compiled)
            .map_err(|_| CodeHostToolsConstructionError::Duplicate)?;
        Ok(Self {
            catalog,
            executor: CodeHostExecutor {
                credentials,
                credential_reference: CredentialReference::new(CODE_HOST_CREDENTIAL_REFERENCE),
                transport,
                credential_unavailable_detail,
                code_host_rejected_detail,
            },
        })
    }

    /// Returns the compiled catalog and executor as separate composition roles.
    pub fn into_parts(
        self,
    ) -> (
        CompiledToolCatalog,
        CodeHostExecutor<Credentials, Transport>,
    ) {
        (self.catalog, self.executor)
    }
}

/// Why the static code-host suite could not be compiled.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CodeHostToolsConstructionError {
    /// One static name was invalid.
    Name,
    /// One static schema was invalid.
    Schema,
    /// One static sanitized error detail was invalid.
    ErrorDetail,
    /// Two static declarations unexpectedly shared one name.
    Duplicate,
}

impl fmt::Display for CodeHostToolsConstructionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("code-host tool suite construction failed")
    }
}

impl Error for CodeHostToolsConstructionError {}

/// Credential-resolving executor for all ten code-host declarations.
#[derive(Clone, Debug)]
pub struct CodeHostExecutor<Credentials, Transport> {
    credentials: Credentials,
    credential_reference: CredentialReference,
    transport: Transport,
    credential_unavailable_detail: ToolExecutionErrorDetail,
    code_host_rejected_detail: ToolExecutionErrorDetail,
}

/// Sanitized code-host executor failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CodeHostExecutorError {
    class: OperatorFailureClass,
}

impl fmt::Display for CodeHostExecutorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("code-host tool executor failed")
    }
}

impl Error for CodeHostExecutorError {}

impl ClassifyOperatorFailure for CodeHostExecutorError {
    fn operator_failure_class(&self) -> OperatorFailureClass {
        self.class
    }
}

impl<Credentials, Transport> ToolExecutor for CodeHostExecutor<Credentials, Transport>
where
    Credentials: CredentialAccess,
    Transport: CodeHostTransport,
{
    type Error = CodeHostExecutorError;

    async fn execute(
        &mut self,
        invocation: ToolExecutionInvocation,
    ) -> Result<CorrelatedToolExecutorEvidence, Self::Error> {
        let Some(kind) = kind_for_name(invocation.request().name().as_str()) else {
            return Err(caller_bug());
        };
        let operation =
            decode_operation(kind, invocation.request().arguments()).map_err(|_| caller_bug())?;
        let credential = match self.credentials.resolve(&self.credential_reference).await {
            Ok(credential) => credential,
            Err(_) => {
                return Ok(invocation.bind(ToolExecutorEvidence::KnownFailed {
                    detail: Some(self.credential_unavailable_detail.clone()),
                }));
            }
        };
        let Some(scrubber) = CredentialScrubber::try_new(&credential) else {
            return Ok(invocation.bind(ToolExecutorEvidence::KnownFailed {
                detail: Some(self.credential_unavailable_detail.clone()),
            }));
        };
        let result = match self.transport.execute(operation, &credential).await {
            Ok(result) if kind.accepts_result(&result) => result,
            Ok(_) => return Err(caller_bug()),
            Err(failure) => {
                if let Some(class) = transport_failure_class(kind, failure) {
                    return Err(CodeHostExecutorError { class });
                }
                let detail = match transport_failure_detail(failure) {
                    KnownFailureDetail::CredentialUnavailable => {
                        self.credential_unavailable_detail.clone()
                    }
                    KnownFailureDetail::CodeHostRejected => self.code_host_rejected_detail.clone(),
                };
                return Ok(invocation.bind(ToolExecutorEvidence::KnownFailed {
                    detail: Some(detail),
                }));
            }
        };
        let mut value = result.into_json_value();
        scrubber.redact_value(&mut value);
        let content = serde_json::to_string(&value).map_err(|_| caller_bug())?;
        if content.len() > result::MAX_ENCODED_RESULT_BYTES {
            if kind.mutates() {
                return Err(CodeHostExecutorError {
                    class: OperatorFailureClass::Infrastructure {
                        commit_ambiguous: true,
                    },
                });
            }
            return Ok(invocation.bind(ToolExecutorEvidence::KnownFailed {
                detail: Some(self.code_host_rejected_detail.clone()),
            }));
        }
        Ok(invocation.bind(ToolExecutorEvidence::CompletedText(content)))
    }
}

/// Which fixed detail a non-fatal transport failure shows the model and the
/// durable transcript.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum KnownFailureDetail {
    /// The daemon-held credential could not authenticate the request.
    CredentialUnavailable,
    /// The code host answered, and its answer ends the attempt.
    CodeHostRejected,
}

/// Shapes a known-failure detail after its class is known to be non-fatal.
///
/// Credential bytes that cannot form the authentication header are a
/// deployment fact the operator can act on, so they read as a credential
/// failure rather than as something the code host decided; the code host never
/// saw the request.
const fn transport_failure_detail(failure: CodeHostTransportFailure) -> KnownFailureDetail {
    match failure {
        CodeHostTransportFailure::InvalidCredential => KnownFailureDetail::CredentialUnavailable,
        CodeHostTransportFailure::Rejected
        | CodeHostTransportFailure::InvalidResponse
        | CodeHostTransportFailure::ResponseTooLarge
        | CodeHostTransportFailure::DispatchUnknown => KnownFailureDetail::CodeHostRejected,
    }
}

const fn transport_failure_class(
    kind: CodeHostToolKind,
    failure: CodeHostTransportFailure,
) -> Option<OperatorFailureClass> {
    match failure {
        CodeHostTransportFailure::InvalidCredential | CodeHostTransportFailure::Rejected => None,
        CodeHostTransportFailure::InvalidResponse | CodeHostTransportFailure::ResponseTooLarge
            if !kind.mutates() =>
        {
            None
        }
        CodeHostTransportFailure::InvalidResponse
        | CodeHostTransportFailure::ResponseTooLarge
        | CodeHostTransportFailure::DispatchUnknown => Some(OperatorFailureClass::Infrastructure {
            commit_ambiguous: kind.mutates(),
        }),
    }
}

const fn caller_bug() -> CodeHostExecutorError {
    CodeHostExecutorError {
        class: OperatorFailureClass::CallerOrHubBug,
    }
}

struct CredentialScrubber {
    exact: String,
    json_escaped: String,
}

impl CredentialScrubber {
    fn try_new(credential: &CredentialValue) -> Option<Self> {
        let exact = std::str::from_utf8(credential.expose_bytes())
            .ok()?
            .to_owned();
        if exact.is_empty() {
            return None;
        }
        let encoded = serde_json::to_string(&exact).ok()?;
        let json_escaped = encoded.get(1..encoded.len().checked_sub(1)?)?.to_owned();
        Some(Self {
            exact,
            json_escaped,
        })
    }

    fn redact_value(&self, value: &mut serde_json::Value) {
        match value {
            serde_json::Value::String(text) => {
                *text = text.replace(&self.exact, "[redacted]");
                if self.json_escaped != self.exact {
                    *text = text.replace(&self.json_escaped, "[redacted]");
                }
            }
            serde_json::Value::Array(values) => {
                for value in values {
                    self.redact_value(value);
                }
            }
            serde_json::Value::Object(object) => {
                for value in object.values_mut() {
                    self.redact_value(value);
                }
            }
            serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use expect_test::Expect;
    use signalbox_application::ToolCatalog;
    use signalbox_domain::ToolName;

    use super::*;

    fn arguments(value: &str) -> NormalizedToolArguments {
        NormalizedToolArguments::try_from_provider_text(value.to_owned())
            .expect("fixture arguments are admitted")
    }

    fn catalog() -> CompiledToolCatalog {
        CodeHostTools::try_new((), ())
            .expect("static code-host declarations compile")
            .into_parts()
            .0
    }

    #[track_caller]
    fn assert_definition(
        catalog: &CompiledToolCatalog,
        name: &str,
        permission: ToolPermissionDefault,
    ) {
        let definition = catalog
            .definition(&ToolName::try_new(name.to_owned()).expect("fixture name is admitted"))
            .expect("fixture declaration exists");
        assert_eq!(definition.permission_default(), permission);
        assert_eq!(definition.effect_class(), ToolEffectClass::ExternalEffect);
    }

    #[track_caller]
    fn assert_schema(name: &str, expected: Expect) {
        let catalog = catalog();
        let definition = catalog
            .definition(&ToolName::try_new(name.to_owned()).expect("fixture name is admitted"))
            .expect("fixture declaration exists");
        let schema: serde_json::Value = serde_json::from_str(definition.input_schema().as_str())
            .expect("registry schema is valid JSON");

        expected.assert_eq(&format!("{schema:#}"));
        assert_eq!(definition.input_schema().as_str(), schema.to_string());
    }

    #[track_caller]
    fn assert_valid(catalog: &CompiledToolCatalog, name: &str, value: &str) {
        let name = ToolName::try_new(name.to_owned()).expect("fixture name is admitted");
        assert_eq!(catalog.validate_arguments(&name, &arguments(value)), Ok(()));
    }

    /// The ten declarations encode the read/mutation approval split and make
    /// every remote observation explicit as an external effect.
    #[test]
    fn code_host_definitions_carry_exact_policy() {
        let catalog = catalog();

        assert_definition(
            &catalog,
            change_request_summary::NAME,
            ToolPermissionDefault::Auto,
        );
        assert_definition(
            &catalog,
            change_request_changed_files::NAME,
            ToolPermissionDefault::Auto,
        );
        assert_definition(
            &catalog,
            change_request_file_patch::NAME,
            ToolPermissionDefault::Auto,
        );
        assert_definition(
            &catalog,
            change_request_checks_status::NAME,
            ToolPermissionDefault::Auto,
        );
        assert_definition(
            &catalog,
            change_request_comment::NAME,
            ToolPermissionDefault::Confirm,
        );
        assert_definition(
            &catalog,
            change_request_review_threads::NAME,
            ToolPermissionDefault::Auto,
        );
        assert_definition(
            &catalog,
            change_request_thread_reply::NAME,
            ToolPermissionDefault::Confirm,
        );
        assert_definition(
            &catalog,
            change_request_thread_resolve::NAME,
            ToolPermissionDefault::Confirm,
        );
        assert_definition(
            &catalog,
            change_request_ci_job_log::NAME,
            ToolPermissionDefault::Auto,
        );
        assert_definition(
            &catalog,
            change_request_rerun_failed_jobs::NAME,
            ToolPermissionDefault::Confirm,
        );
    }

    /// Complete model-facing schema for `change_request_summary`.
    #[test]
    fn change_request_summary_schema_is_the_exact_wire_artifact() {
        assert_schema(
            change_request_summary::NAME,
            expect_test::expect![[r#"
            {
              "additionalProperties": false,
              "properties": {
                "number": {
                  "description": "Change-request number.",
                  "maximum": 2147483647,
                  "minimum": 1,
                  "type": "integer"
                },
                "repository": {
                  "description": "Exact owner/repository spelling.",
                  "maxLength": 256,
                  "pattern": "^(?:[A-Za-z0-9_-]|\\.[A-Za-z0-9_-]|\\.\\.[A-Za-z0-9._-])[A-Za-z0-9._-]*/(?:[A-Za-z0-9_-]|\\.[A-Za-z0-9_-]|\\.\\.[A-Za-z0-9._-])[A-Za-z0-9._-]*$",
                  "type": "string"
                }
              },
              "required": [
                "repository",
                "number"
              ],
              "type": "object"
            }"#]],
        );
    }

    /// Complete model-facing schema for `change_request_changed_files`.
    #[test]
    fn change_request_changed_files_schema_is_the_exact_wire_artifact() {
        assert_schema(
            change_request_changed_files::NAME,
            expect_test::expect![[r#"
                {
                  "additionalProperties": false,
                  "properties": {
                    "number": {
                      "description": "Change-request number.",
                      "maximum": 2147483647,
                      "minimum": 1,
                      "type": "integer"
                    },
                    "repository": {
                      "description": "Exact owner/repository spelling.",
                      "maxLength": 256,
                      "pattern": "^(?:[A-Za-z0-9_-]|\\.[A-Za-z0-9_-]|\\.\\.[A-Za-z0-9._-])[A-Za-z0-9._-]*/(?:[A-Za-z0-9_-]|\\.[A-Za-z0-9_-]|\\.\\.[A-Za-z0-9._-])[A-Za-z0-9._-]*$",
                      "type": "string"
                    }
                  },
                  "required": [
                    "repository",
                    "number"
                  ],
                  "type": "object"
                }"#]],
        );
    }

    /// Complete model-facing schema for `change_request_file_patch`.
    #[test]
    fn change_request_file_patch_schema_is_the_exact_wire_artifact() {
        assert_schema(
            change_request_file_patch::NAME,
            expect_test::expect![[r#"
                {
                  "additionalProperties": false,
                  "properties": {
                    "number": {
                      "description": "Change-request number.",
                      "maximum": 2147483647,
                      "minimum": 1,
                      "type": "integer"
                    },
                    "path": {
                      "description": "Exact repository-relative changed path.",
                      "maxLength": 4096,
                      "minLength": 1,
                      "pattern": "^[^/\\u0000][^\\u0000]*$",
                      "type": "string"
                    },
                    "repository": {
                      "description": "Exact owner/repository spelling.",
                      "maxLength": 256,
                      "pattern": "^(?:[A-Za-z0-9_-]|\\.[A-Za-z0-9_-]|\\.\\.[A-Za-z0-9._-])[A-Za-z0-9._-]*/(?:[A-Za-z0-9_-]|\\.[A-Za-z0-9_-]|\\.\\.[A-Za-z0-9._-])[A-Za-z0-9._-]*$",
                      "type": "string"
                    }
                  },
                  "required": [
                    "repository",
                    "number",
                    "path"
                  ],
                  "type": "object"
                }"#]],
        );
    }

    /// Complete model-facing schema for `change_request_checks_status`.
    #[test]
    fn change_request_checks_status_schema_is_the_exact_wire_artifact() {
        assert_schema(
            change_request_checks_status::NAME,
            expect_test::expect![[r#"
                {
                  "additionalProperties": false,
                  "properties": {
                    "repository": {
                      "description": "Exact owner/repository spelling.",
                      "maxLength": 256,
                      "pattern": "^(?:[A-Za-z0-9_-]|\\.[A-Za-z0-9_-]|\\.\\.[A-Za-z0-9._-])[A-Za-z0-9._-]*/(?:[A-Za-z0-9_-]|\\.[A-Za-z0-9_-]|\\.\\.[A-Za-z0-9._-])[A-Za-z0-9._-]*$",
                      "type": "string"
                    },
                    "revision": {
                      "description": "Exact lowercase 40-hex head revision.",
                      "maxLength": 40,
                      "minLength": 40,
                      "pattern": "^[0-9a-f]{40}$",
                      "type": "string"
                    }
                  },
                  "required": [
                    "repository",
                    "revision"
                  ],
                  "type": "object"
                }"#]],
        );
    }

    /// Complete model-facing schema for `change_request_comment`.
    #[test]
    fn change_request_comment_schema_is_the_exact_wire_artifact() {
        assert_schema(
            change_request_comment::NAME,
            expect_test::expect![[r#"
            {
              "additionalProperties": false,
              "properties": {
                "body": {
                  "description": "Exact nonempty comment body.",
                  "maxLength": 65536,
                  "minLength": 1,
                  "pattern": "^[^\\u0000]+$",
                  "type": "string"
                },
                "number": {
                  "description": "Change-request number.",
                  "maximum": 2147483647,
                  "minimum": 1,
                  "type": "integer"
                },
                "repository": {
                  "description": "Exact owner/repository spelling.",
                  "maxLength": 256,
                  "pattern": "^(?:[A-Za-z0-9_-]|\\.[A-Za-z0-9_-]|\\.\\.[A-Za-z0-9._-])[A-Za-z0-9._-]*/(?:[A-Za-z0-9_-]|\\.[A-Za-z0-9_-]|\\.\\.[A-Za-z0-9._-])[A-Za-z0-9._-]*$",
                  "type": "string"
                }
              },
              "required": [
                "repository",
                "number",
                "body"
              ],
              "type": "object"
            }"#]],
        );
    }

    /// Complete model-facing schema for `change_request_review_threads`.
    #[test]
    fn change_request_review_threads_schema_is_the_exact_wire_artifact() {
        assert_schema(
            change_request_review_threads::NAME,
            expect_test::expect![[r#"
                {
                  "additionalProperties": false,
                  "properties": {
                    "number": {
                      "description": "Change-request number.",
                      "maximum": 2147483647,
                      "minimum": 1,
                      "type": "integer"
                    },
                    "repository": {
                      "description": "Exact owner/repository spelling.",
                      "maxLength": 256,
                      "pattern": "^(?:[A-Za-z0-9_-]|\\.[A-Za-z0-9_-]|\\.\\.[A-Za-z0-9._-])[A-Za-z0-9._-]*/(?:[A-Za-z0-9_-]|\\.[A-Za-z0-9_-]|\\.\\.[A-Za-z0-9._-])[A-Za-z0-9._-]*$",
                      "type": "string"
                    }
                  },
                  "required": [
                    "repository",
                    "number"
                  ],
                  "type": "object"
                }"#]],
        );
    }

    /// Complete model-facing schema for `change_request_thread_reply`.
    #[test]
    fn change_request_thread_reply_schema_is_the_exact_wire_artifact() {
        assert_schema(
            change_request_thread_reply::NAME,
            expect_test::expect![[r#"
                {
                  "additionalProperties": false,
                  "properties": {
                    "body": {
                      "description": "Exact nonempty reply body.",
                      "maxLength": 65536,
                      "minLength": 1,
                      "pattern": "^[^\\u0000]+$",
                      "type": "string"
                    },
                    "thread_id": {
                      "description": "Opaque review-thread node identity.",
                      "maxLength": 512,
                      "minLength": 1,
                      "pattern": "^[^\\u0000-\\u001F\\u007F-\\u009F]+$",
                      "type": "string"
                    }
                  },
                  "required": [
                    "thread_id",
                    "body"
                  ],
                  "type": "object"
                }"#]],
        );
    }

    /// Complete model-facing schema for `change_request_thread_resolve`.
    #[test]
    fn change_request_thread_resolve_schema_is_the_exact_wire_artifact() {
        assert_schema(
            change_request_thread_resolve::NAME,
            expect_test::expect![[r#"
                {
                  "additionalProperties": false,
                  "properties": {
                    "thread_id": {
                      "description": "Opaque review-thread node identity.",
                      "maxLength": 512,
                      "minLength": 1,
                      "pattern": "^[^\\u0000-\\u001F\\u007F-\\u009F]+$",
                      "type": "string"
                    }
                  },
                  "required": [
                    "thread_id"
                  ],
                  "type": "object"
                }"#]],
        );
    }

    /// Complete model-facing schema for `change_request_ci_job_log`.
    #[test]
    fn change_request_ci_job_log_schema_is_the_exact_wire_artifact() {
        assert_schema(
            change_request_ci_job_log::NAME,
            expect_test::expect![[r#"
                {
                  "additionalProperties": false,
                  "properties": {
                    "job_id": {
                      "description": "GitHub Actions job identity.",
                      "maximum": 18446744073709551615,
                      "minimum": 1,
                      "type": "integer"
                    },
                    "repository": {
                      "description": "Exact owner/repository spelling.",
                      "maxLength": 256,
                      "pattern": "^(?:[A-Za-z0-9_-]|\\.[A-Za-z0-9_-]|\\.\\.[A-Za-z0-9._-])[A-Za-z0-9._-]*/(?:[A-Za-z0-9_-]|\\.[A-Za-z0-9_-]|\\.\\.[A-Za-z0-9._-])[A-Za-z0-9._-]*$",
                      "type": "string"
                    }
                  },
                  "required": [
                    "repository",
                    "job_id"
                  ],
                  "type": "object"
                }"#]],
        );
    }

    /// Complete model-facing schema for `change_request_rerun_failed_jobs`.
    #[test]
    fn change_request_rerun_failed_jobs_schema_is_the_exact_wire_artifact() {
        assert_schema(
            change_request_rerun_failed_jobs::NAME,
            expect_test::expect![[r#"
                {
                  "additionalProperties": false,
                  "properties": {
                    "repository": {
                      "description": "Exact owner/repository spelling.",
                      "maxLength": 256,
                      "pattern": "^(?:[A-Za-z0-9_-]|\\.[A-Za-z0-9_-]|\\.\\.[A-Za-z0-9._-])[A-Za-z0-9._-]*/(?:[A-Za-z0-9_-]|\\.[A-Za-z0-9_-]|\\.\\.[A-Za-z0-9._-])[A-Za-z0-9._-]*$",
                      "type": "string"
                    },
                    "run_id": {
                      "description": "GitHub Actions workflow-run identity.",
                      "maximum": 18446744073709551615,
                      "minimum": 1,
                      "type": "integer"
                    }
                  },
                  "required": [
                    "repository",
                    "run_id"
                  ],
                  "type": "object"
                }"#]],
        );
    }

    /// The result URL newtype advertises the absolute HTTPS shape and sound
    /// code-point bound corresponding to its checked byte cap.
    #[test]
    fn code_host_url_schema_states_its_checked_shape() {
        let schema = <CodeHostUrl as schemars::JsonSchema>::json_schema(
            &mut schemars::SchemaGenerator::default(),
        );

        expect_test::expect![[r#"
            {
              "format": "uri",
              "maxLength": 8192,
              "pattern": "^https://[^/@\\u0000-\\u0020\\u007F-\\u009F]+(?:[/?#]|$)",
              "type": "string"
            }"#]]
        .assert_eq(&format!("{:#}", schema.to_value()));
    }

    /// Summary arguments decode into a checked repository and positive number.
    #[test]
    fn change_request_summary_typed_decode_accepts_exact_shape() {
        assert_valid(
            &catalog(),
            change_request_summary::NAME,
            r#"{"number":17,"repository":"owner/repository"}"#,
        );
    }

    /// Changed-files arguments decode into a checked repository and number.
    #[test]
    fn change_request_changed_files_typed_decode_accepts_exact_shape() {
        assert_valid(
            &catalog(),
            change_request_changed_files::NAME,
            r#"{"number":17,"repository":"owner/repository"}"#,
        );
    }

    /// File-patch arguments retain one bounded repository-relative path.
    #[test]
    fn change_request_file_patch_typed_decode_accepts_exact_shape() {
        assert_valid(
            &catalog(),
            change_request_file_patch::NAME,
            r#"{"number":17,"path":"src/lib.rs","repository":"owner/repository"}"#,
        );
    }

    /// File-patch arguments preserve an exact Git path containing a
    /// JSON-representable control character.
    #[test]
    fn change_request_file_patch_typed_decode_accepts_newline_path() {
        assert_valid(
            &catalog(),
            change_request_file_patch::NAME,
            r#"{"number":17,"path":"src/line\nbreak.rs","repository":"owner/repository"}"#,
        );
    }

    /// Check-status arguments require one lowercase forty-hex revision.
    #[test]
    fn change_request_checks_status_typed_decode_accepts_exact_shape() {
        assert_valid(
            &catalog(),
            change_request_checks_status::NAME,
            r#"{"repository":"owner/repository","revision":"0123456789abcdef0123456789abcdef01234567"}"#,
        );
    }

    /// Comment arguments retain one bounded nonempty body.
    #[test]
    fn change_request_comment_typed_decode_accepts_exact_shape() {
        assert_valid(
            &catalog(),
            change_request_comment::NAME,
            r#"{"body":"reviewed","number":17,"repository":"owner/repository"}"#,
        );
    }

    /// Review-thread arguments decode into a checked repository and number.
    #[test]
    fn change_request_review_threads_typed_decode_accepts_exact_shape() {
        assert_valid(
            &catalog(),
            change_request_review_threads::NAME,
            r#"{"number":17,"repository":"owner/repository"}"#,
        );
    }

    /// Thread-reply arguments retain opaque identity and bounded body.
    #[test]
    fn change_request_thread_reply_typed_decode_accepts_exact_shape() {
        assert_valid(
            &catalog(),
            change_request_thread_reply::NAME,
            r#"{"body":"fixed","thread_id":"PRRT_node"}"#,
        );
    }

    /// Thread-resolve arguments retain one opaque node identity.
    #[test]
    fn change_request_thread_resolve_typed_decode_accepts_exact_shape() {
        assert_valid(
            &catalog(),
            change_request_thread_resolve::NAME,
            r#"{"thread_id":"PRRT_node"}"#,
        );
    }

    /// CI-job-log arguments retain one positive job identity.
    #[test]
    fn change_request_ci_job_log_typed_decode_accepts_exact_shape() {
        assert_valid(
            &catalog(),
            change_request_ci_job_log::NAME,
            r#"{"job_id":9001,"repository":"owner/repository"}"#,
        );
    }

    /// Rerun arguments retain one positive workflow-run identity.
    #[test]
    fn change_request_rerun_failed_jobs_typed_decode_accepts_exact_shape() {
        assert_valid(
            &catalog(),
            change_request_rerun_failed_jobs::NAME,
            r#"{"repository":"owner/repository","run_id":7001}"#,
        );
    }

    /// INV-035: credential text and its JSON-escaped spelling are scrubbed
    /// recursively before a successful result can enter durable tool evidence.
    #[test]
    fn credential_scrubber_redacts_exact_and_json_escaped_values() {
        let credential = CredentialValue::new(b"secret\"line".to_vec());
        let scrubber =
            CredentialScrubber::try_new(&credential).expect("fixture credential is usable");
        let mut value = serde_json::json!({
            "exact": "prefix secret\"line suffix",
            "escaped": r#"prefix secret\"line suffix"#,
            "nested": ["secret\"line"],
        });

        scrubber.redact_value(&mut value);

        assert_eq!(
            value,
            serde_json::json!({
                "exact": "prefix [redacted] suffix",
                "escaped": "prefix [redacted] suffix",
                "nested": ["[redacted]"],
            })
        );
    }

    /// A malformed acknowledgement after a mutation is fail-closed as
    /// commit-ambiguous rather than durable known-failure evidence.
    #[test]
    fn mutation_invalid_response_is_commit_ambiguous() {
        assert_eq!(
            transport_failure_class(
                CodeHostToolKind::ThreadResolve,
                CodeHostTransportFailure::InvalidResponse,
            ),
            Some(OperatorFailureClass::Infrastructure {
                commit_ambiguous: true
            })
        );
    }

    /// A malformed read response cannot imply a durable remote mutation.
    #[test]
    fn read_invalid_response_is_known_failure() {
        assert_eq!(
            transport_failure_class(
                CodeHostToolKind::ReviewThreads,
                CodeHostTransportFailure::InvalidResponse,
            ),
            None
        );
    }

    /// Credential bytes that cannot form the authentication header never reach
    /// the code host, so the detail names the credential rather than blaming a
    /// decision the code host never made.
    #[test]
    fn invalid_credential_presents_the_credential_detail() {
        assert_eq!(
            transport_failure_detail(CodeHostTransportFailure::InvalidCredential),
            KnownFailureDetail::CredentialUnavailable
        );
    }

    /// A definitive answer from the code host keeps the rejection detail, so
    /// the credential detail stays a credential claim.
    #[test]
    fn rejected_request_presents_the_code_host_detail() {
        assert_eq!(
            transport_failure_detail(CodeHostTransportFailure::Rejected),
            KnownFailureDetail::CodeHostRejected
        );
    }

    /// A bounded response the typed contract refuses is the code host's
    /// answer, not a credential fact.
    #[test]
    fn invalid_response_presents_the_code_host_detail() {
        assert_eq!(
            transport_failure_detail(CodeHostTransportFailure::InvalidResponse),
            KnownFailureDetail::CodeHostRejected
        );
    }

    /// A returned node identity the argument side would refuse never enters
    /// durable evidence, so every identity a result offers can be passed back.
    #[test]
    fn returned_node_ids_reject_identities_the_argument_side_refuses() {
        let oversized = "N".repeat(arguments::MAX_OPAQUE_ID_BYTES + 1);

        let comment = ReviewThreadComment::try_new(
            oversized.clone(),
            Some(String::from("reviewer")),
            String::from("please adjust"),
            String::from("https://github.example/comment/7001"),
        );
        let thread = ReviewThread::try_new(ReviewThreadFields {
            id: oversized.clone(),
            resolved: false,
            outdated: false,
            path: String::from("src/lib.rs"),
            line: Some(12),
            comments: Vec::new(),
            comments_truncated: false,
        });
        let reply = ThreadReplyResult::try_new(
            oversized.clone(),
            String::from("https://github.example/comment/7002"),
        );

        assert!(
            CodeHostOpaqueId::try_new(oversized).is_err(),
            "the argument side must refuse the identity these results are checked against"
        );
        assert_eq!(comment, None);
        assert_eq!(thread, None);
        assert_eq!(reply, None);
    }

    /// A returned head revision the checks argument would refuse never enters
    /// durable evidence as a successful summary.
    #[test]
    fn returned_summary_revision_rejects_revisions_the_argument_side_refuses() {
        let malformed = String::from("HEAD");

        let summary = ChangeRequestSummaryResult::try_new(ChangeRequestSummaryFields {
            number: 17,
            title: String::from("summary"),
            body: None,
            state: String::from("open"),
            draft: false,
            author: None,
            base_ref: String::from("main"),
            head_ref: String::from("feature"),
            head_revision: malformed.clone(),
            url: String::from("https://github.example/owner/repository/pull/17"),
        });

        assert!(
            CodeHostRevision::try_new(malformed).is_err(),
            "the argument side must refuse the revision this result is checked against"
        );
        assert_eq!(summary, None);
    }

    /// A success-shaped acknowledgement whose URL is not an absolute HTTPS
    /// location cannot become durable completed evidence.
    #[test]
    fn returned_urls_reject_non_absolute_https_values() {
        let whitespace = ChangeRequestCommentResult::try_new(8001, String::from(" "));
        let script = ChangeRequestCommentResult::try_new(8001, String::from("javascript:0"));
        let relative = ChangeRequestCommentResult::try_new(8001, String::from("/comment/8001"));
        let insecure =
            ChangeRequestCommentResult::try_new(8001, String::from("http://github.example/8001"));
        let absolute = ChangeRequestCommentResult::try_new(
            8001,
            String::from("https://github.example/comment/8001"),
        );

        assert_eq!(whitespace, None);
        assert_eq!(script, None);
        assert_eq!(relative, None);
        assert_eq!(insecure, None);
        assert!(absolute.is_some());
    }

    /// A resolve result exists only for an acknowledgement that establishes
    /// the requested terminal posture.
    #[test]
    fn thread_resolve_result_rejects_open_acknowledgement() {
        assert_eq!(
            ThreadResolveResult::try_new(String::from("PRRT_node"), ReviewThreadResolution::Open,),
            None
        );
    }
}
