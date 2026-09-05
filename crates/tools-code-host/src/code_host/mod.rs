//! Credentialed daemon-side code-host review tools.

mod arguments;
mod change_request_changed_files;
mod change_request_checks_status;
mod change_request_ci_job_log;
mod change_request_comment;
mod change_request_file_patch;
mod change_request_rerun_failed_jobs;
mod change_request_review_threads;
mod change_request_stack_state;
mod change_request_summary;
mod change_request_thread_inventory;
mod change_request_thread_reply;
mod change_request_thread_resolve;
mod github;
mod repository_list_directory;
mod repository_read_file;
mod repository_result;
mod result;
mod review_slog;

use std::{error::Error, fmt, future::Future, time::Duration};

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
    CodeHostChangeRequestNumber, CodeHostCommentBody, CodeHostCursor, CodeHostFilePath,
    CodeHostOpaqueId, CodeHostRepository, CodeHostRevision,
};
pub use change_request_changed_files::ChangedFilesArguments;
pub use change_request_checks_status::ChecksStatusArguments;
pub use change_request_ci_job_log::CiJobLogArguments;
pub use change_request_comment::ChangeRequestCommentArguments;
pub use change_request_file_patch::FilePatchArguments;
pub use change_request_rerun_failed_jobs::RerunFailedJobsArguments;
pub use change_request_review_threads::ReviewThreadsArguments;
pub use change_request_stack_state::StackStateArguments;
pub use change_request_summary::ChangeRequestSummaryArguments;
pub use change_request_thread_inventory::ThreadInventoryArguments;
pub use change_request_thread_reply::ThreadReplyArguments;
pub use change_request_thread_resolve::ThreadResolveArguments;
pub use github::{GitHubCodeHostConstructionError, GitHubCodeHostTransport};
pub use repository_list_directory::RepositoryListDirectoryArguments;
pub use repository_read_file::{RepositoryLineRange, RepositoryReadFileArguments};
pub use repository_result::{
    RepositoryDirectoryEntry, RepositoryFileContentFields, RepositoryListDirectoryResult,
    RepositoryObjectKind, RepositoryReadFileResult,
};
pub use result::{
    ChangeRequestCommentResult, ChangeRequestSummaryFields, ChangeRequestSummaryResult,
    ChangedFile, ChangedFilesResult, CheckStatus, ChecksStatusResult, CiJobLogResult,
    CodeHostResult, CodeHostResultCompleteness, CodeHostUrl, FilePatchResult,
    RerunFailedJobsResult, ReviewThread, ReviewThreadComment, ReviewThreadFields,
    ReviewThreadResolution, ReviewThreadsResult, ThreadReplyResult, ThreadResolveResult,
};
pub use review_slog::{
    ChildStackState, ESCALATION_MARKER, ReviewAuthorClass, ReviewDispositionClass,
    ReviewThreadInventoryFields, ReviewThreadInventoryItem, StackStateFields, StackStateResult,
    ThreadInventoryResult,
};

/// Non-secret name of the daemon-held code-host credential.
pub const CODE_HOST_CREDENTIAL_REFERENCE: &str = "github-primary";

/// Deployment-supplied policy bounds for code-host operations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CodeHostNumericBounds {
    request_timeout: Option<Duration>,
    job_log_bytes: Option<usize>,
    stack_comparisons_in_flight: Option<usize>,
    result_text_bytes: Option<usize>,
    result_items: Option<usize>,
    repository_file_content_bytes: Option<usize>,
}

impl CodeHostNumericBounds {
    /// Records every required code-host policy bound. `None` means unbounded.
    pub const fn new(
        request_timeout: Option<Duration>,
        job_log_bytes: Option<usize>,
        stack_comparisons_in_flight: Option<usize>,
        result_text_bytes: Option<usize>,
        result_items: Option<usize>,
        repository_file_content_bytes: Option<usize>,
    ) -> Self {
        Self {
            request_timeout,
            job_log_bytes,
            stack_comparisons_in_flight,
            result_text_bytes,
            result_items,
            repository_file_content_bytes,
        }
    }

    const fn request_timeout(self) -> Option<Duration> {
        self.request_timeout
    }

    const fn job_log_bytes(self) -> Option<usize> {
        self.job_log_bytes
    }

    const fn stack_comparisons_in_flight(self) -> Option<usize> {
        self.stack_comparisons_in_flight
    }

    const fn repository_file_content_bytes(self) -> Option<usize> {
        self.repository_file_content_bytes
    }

    const fn result_text_bytes(self) -> Option<usize> {
        self.result_text_bytes
    }

    const fn result_items(self) -> Option<usize> {
        self.result_items
    }

    fn permits_result_text(self, observed: usize) -> bool {
        self.result_text_bytes.is_none_or(|limit| observed <= limit)
    }

    fn permits_result_items(self, observed: usize) -> bool {
        self.result_items.is_none_or(|limit| observed <= limit)
    }
}

#[cfg(test)]
pub(super) const fn test_numeric_bounds() -> CodeHostNumericBounds {
    CodeHostNumericBounds::new(None, None, None, None, None, None)
}

/// Registry name for change-request summary lookup.
pub const CHANGE_REQUEST_SUMMARY_NAME: &str = change_request_summary::NAME;
/// Registry name for changed-files lookup.
pub const CHANGE_REQUEST_CHANGED_FILES_NAME: &str = change_request_changed_files::NAME;
/// Registry name for per-file patch lookup.
pub const CHANGE_REQUEST_FILE_PATCH_NAME: &str = change_request_file_patch::NAME;
/// Registry name for exact-revision repository file reads.
pub const REPOSITORY_READ_FILE_NAME: &str = repository_read_file::NAME;
/// Registry name for exact-revision repository directory listings.
pub const REPOSITORY_LIST_DIRECTORY_NAME: &str = repository_list_directory::NAME;
/// Registry name for check-status lookup.
pub const CHANGE_REQUEST_CHECKS_STATUS_NAME: &str = change_request_checks_status::NAME;
/// Registry name for top-level comment creation.
pub const CHANGE_REQUEST_COMMENT_NAME: &str = change_request_comment::NAME;
/// Registry name for review-thread lookup.
pub const CHANGE_REQUEST_REVIEW_THREADS_NAME: &str = change_request_review_threads::NAME;
/// Registry name for stack ancestry lookup.
pub const CHANGE_REQUEST_STACK_STATE_NAME: &str = change_request_stack_state::NAME;
/// Registry name for structured review-thread inventory.
pub const CHANGE_REQUEST_THREAD_INVENTORY_NAME: &str = change_request_thread_inventory::NAME;
/// Registry name for review-thread replies.
pub const CHANGE_REQUEST_THREAD_REPLY_NAME: &str = change_request_thread_reply::NAME;
/// Registry name for review-thread resolution.
pub const CHANGE_REQUEST_THREAD_RESOLVE_NAME: &str = change_request_thread_resolve::NAME;
/// Registry name for CI job-log lookup.
pub const CHANGE_REQUEST_CI_JOB_LOG_NAME: &str = change_request_ci_job_log::NAME;
/// Registry name for failed-job reruns.
pub const CHANGE_REQUEST_RERUN_FAILED_JOBS_NAME: &str = change_request_rerun_failed_jobs::NAME;

/// Registry names in deterministic catalog order.
pub const CODE_HOST_TOOL_NAMES: [&str; 14] = [
    CHANGE_REQUEST_CHANGED_FILES_NAME,
    CHANGE_REQUEST_CHECKS_STATUS_NAME,
    CHANGE_REQUEST_CI_JOB_LOG_NAME,
    CHANGE_REQUEST_COMMENT_NAME,
    CHANGE_REQUEST_FILE_PATCH_NAME,
    CHANGE_REQUEST_RERUN_FAILED_JOBS_NAME,
    CHANGE_REQUEST_REVIEW_THREADS_NAME,
    CHANGE_REQUEST_STACK_STATE_NAME,
    CHANGE_REQUEST_SUMMARY_NAME,
    CHANGE_REQUEST_THREAD_INVENTORY_NAME,
    CHANGE_REQUEST_THREAD_REPLY_NAME,
    CHANGE_REQUEST_THREAD_RESOLVE_NAME,
    REPOSITORY_LIST_DIRECTORY_NAME,
    REPOSITORY_READ_FILE_NAME,
];

const INVALID_ARGUMENTS_DETAIL: &str = "code-host tool arguments are invalid";
const CREDENTIAL_UNAVAILABLE_DETAIL: &str = "code-host credential is unavailable";
const CODE_HOST_REJECTED_DETAIL: &str =
    "code host rejected the request or returned an invalid bounded response";
const CHANGED_FILE_NOT_FOUND_DETAIL: &str =
    "requested changed file was not found in the change request";
const THREAD_NOT_IN_CHANGE_REQUEST_DETAIL: &str =
    "requested review thread was not found in the named change request";

macro_rules! define_code_host_tool_kinds {
    ($( $(#[$attribute:meta])* $variant:ident),+ $(,)?) => {
        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        enum CodeHostToolKind {
            $($(#[$attribute])* $variant),+
        }

        impl CodeHostToolKind {
            const ALL: &'static [Self] = &[$(Self::$variant),+];
        }
    };
}

define_code_host_tool_kinds! {
    /// Registry effect posture: read-only, `Auto`, and `ExternalEffect`
    /// because the code host observes the authenticated request.
    Summary,
    /// Registry effect posture: read-only, `Auto`, and `ExternalEffect`.
    ChangedFiles,
    /// Registry effect posture: read-only, `Auto`, and `ExternalEffect`.
    FilePatch,
    /// Registry effect posture: read-only, `Auto`, and `ExternalEffect`.
    ListDirectory,
    /// Registry effect posture: read-only, `Auto`, and `ExternalEffect`.
    ReadFile,
    /// Registry effect posture: read-only, `Auto`, and `ExternalEffect`.
    ChecksStatus,
    /// Registry effect posture: mutation, `Confirm`, and `ExternalEffect`.
    Comment,
    /// Registry effect posture: read-only, `Auto`, and `ExternalEffect`.
    ReviewThreads,
    /// Registry effect posture: read-only, `Auto`, and `ExternalEffect`.
    StackState,
    /// Registry effect posture: read-only, `Auto`, and `ExternalEffect`.
    ThreadInventory,
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
    const fn name(self) -> &'static str {
        match self {
            Self::Summary => change_request_summary::NAME,
            Self::ChangedFiles => change_request_changed_files::NAME,
            Self::FilePatch => change_request_file_patch::NAME,
            Self::ListDirectory => repository_list_directory::NAME,
            Self::ReadFile => repository_read_file::NAME,
            Self::ChecksStatus => change_request_checks_status::NAME,
            Self::Comment => change_request_comment::NAME,
            Self::ReviewThreads => change_request_review_threads::NAME,
            Self::StackState => change_request_stack_state::NAME,
            Self::ThreadInventory => change_request_thread_inventory::NAME,
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
            Self::ListDirectory => {
                compile_contract_definition::<repository_list_directory::Contract>(
                    permission,
                    ToolEffectClass::ExternalEffect,
                )
            }
            Self::ReadFile => compile_contract_definition::<repository_read_file::Contract>(
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
            Self::StackState => {
                compile_contract_definition::<change_request_stack_state::Contract>(
                    permission,
                    ToolEffectClass::ExternalEffect,
                )
            }
            Self::ThreadInventory => compile_contract_definition::<
                change_request_thread_inventory::Contract,
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
            | Self::ListDirectory
            | Self::ReadFile
            | Self::ChecksStatus
            | Self::ReviewThreads
            | Self::StackState
            | Self::ThreadInventory
            | Self::CiJobLog => ToolPermissionDefault::Auto,
        }
    }

    const fn mutates(self) -> bool {
        match self {
            Self::Comment | Self::ThreadReply | Self::ThreadResolve | Self::RerunFailedJobs => true,
            Self::Summary
            | Self::ChangedFiles
            | Self::FilePatch
            | Self::ListDirectory
            | Self::ReadFile
            | Self::ChecksStatus
            | Self::ReviewThreads
            | Self::StackState
            | Self::ThreadInventory
            | Self::CiJobLog => false,
        }
    }

    fn accepts_result(self, result: &CodeHostResult) -> bool {
        let result_kind = match result {
            CodeHostResult::Summary(_) => Self::Summary,
            CodeHostResult::ChangedFiles(_) => Self::ChangedFiles,
            CodeHostResult::FilePatch(_) => Self::FilePatch,
            CodeHostResult::ListDirectory(_) => Self::ListDirectory,
            CodeHostResult::ReadFile(_) => Self::ReadFile,
            CodeHostResult::ChecksStatus(_) => Self::ChecksStatus,
            CodeHostResult::Comment(_) => Self::Comment,
            CodeHostResult::ReviewThreads(_) => Self::ReviewThreads,
            CodeHostResult::StackState(_) => Self::StackState,
            CodeHostResult::ThreadInventory(_) => Self::ThreadInventory,
            CodeHostResult::ThreadReply(_) => Self::ThreadReply,
            CodeHostResult::ThreadResolve(_) => Self::ThreadResolve,
            CodeHostResult::CiJobLog(_) => Self::CiJobLog,
            CodeHostResult::RerunFailedJobs(_) => Self::RerunFailedJobs,
        };
        match self {
            Self::Summary
            | Self::ChangedFiles
            | Self::FilePatch
            | Self::ListDirectory
            | Self::ReadFile
            | Self::ChecksStatus
            | Self::Comment
            | Self::ReviewThreads
            | Self::StackState
            | Self::ThreadInventory
            | Self::ThreadReply
            | Self::ThreadResolve
            | Self::CiJobLog
            | Self::RerunFailedJobs => self == result_kind,
        }
    }
}

fn kind_for_name(name: &str) -> Option<CodeHostToolKind> {
    CodeHostToolKind::ALL
        .iter()
        .copied()
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
    /// List one directory at an exact revision.
    ListDirectory(RepositoryListDirectoryArguments),
    /// Read one file at an exact revision.
    ReadFile(RepositoryReadFileArguments),
    /// Read checks for one frozen revision.
    ChecksStatus(ChecksStatusArguments),
    /// Post one top-level comment.
    Comment(ChangeRequestCommentArguments),
    /// Read one review-thread page.
    ReviewThreads(ReviewThreadsArguments),
    /// Read parent and immediate-child stack ancestry.
    StackState(StackStateArguments),
    /// Read one structured review-thread inventory page.
    ThreadInventory(ThreadInventoryArguments),
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
            Self::ListDirectory(_) => repository_list_directory::NAME,
            Self::ReadFile(_) => repository_read_file::NAME,
            Self::ChecksStatus(_) => change_request_checks_status::NAME,
            Self::Comment(_) => change_request_comment::NAME,
            Self::ReviewThreads(_) => change_request_review_threads::NAME,
            Self::StackState(_) => change_request_stack_state::NAME,
            Self::ThreadInventory(_) => change_request_thread_inventory::NAME,
            Self::ThreadReply(_) => change_request_thread_reply::NAME,
            Self::ThreadResolve(_) => change_request_thread_resolve::NAME,
            Self::CiJobLog(_) => change_request_ci_job_log::NAME,
            Self::RerunFailedJobs(_) => change_request_rerun_failed_jobs::NAME,
        }
    }

    fn accepts_repository_result(&self, result: &CodeHostResult) -> bool {
        match self {
            Self::ReadFile(arguments) => match result {
                CodeHostResult::ReadFile(result) => result.matches(arguments),
                CodeHostResult::Summary(_)
                | CodeHostResult::ChangedFiles(_)
                | CodeHostResult::FilePatch(_)
                | CodeHostResult::ListDirectory(_)
                | CodeHostResult::ChecksStatus(_)
                | CodeHostResult::Comment(_)
                | CodeHostResult::ReviewThreads(_)
                | CodeHostResult::ThreadReply(_)
                | CodeHostResult::ThreadResolve(_)
                | CodeHostResult::CiJobLog(_)
                | CodeHostResult::RerunFailedJobs(_)
                | CodeHostResult::StackState(_)
                | CodeHostResult::ThreadInventory(_) => false,
            },
            Self::ListDirectory(arguments) => match result {
                CodeHostResult::ListDirectory(result) => result.matches(arguments),
                CodeHostResult::Summary(_)
                | CodeHostResult::ChangedFiles(_)
                | CodeHostResult::FilePatch(_)
                | CodeHostResult::ReadFile(_)
                | CodeHostResult::ChecksStatus(_)
                | CodeHostResult::Comment(_)
                | CodeHostResult::ReviewThreads(_)
                | CodeHostResult::ThreadReply(_)
                | CodeHostResult::ThreadResolve(_)
                | CodeHostResult::CiJobLog(_)
                | CodeHostResult::RerunFailedJobs(_)
                | CodeHostResult::StackState(_)
                | CodeHostResult::ThreadInventory(_) => false,
            },
            Self::Summary(_)
            | Self::ChangedFiles(_)
            | Self::FilePatch(_)
            | Self::ChecksStatus(_)
            | Self::Comment(_)
            | Self::ReviewThreads(_)
            | Self::StackState(_)
            | Self::ThreadInventory(_)
            | Self::ThreadReply(_)
            | Self::ThreadResolve(_)
            | Self::CiJobLog(_)
            | Self::RerunFailedJobs(_) => true,
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
        CodeHostToolKind::ListDirectory => repository_list_directory::decode(arguments),
        CodeHostToolKind::ReadFile => repository_read_file::decode(arguments),
        CodeHostToolKind::ChecksStatus => change_request_checks_status::decode(arguments),
        CodeHostToolKind::Comment => change_request_comment::decode(arguments),
        CodeHostToolKind::ReviewThreads => change_request_review_threads::decode(arguments),
        CodeHostToolKind::StackState => change_request_stack_state::decode(arguments),
        CodeHostToolKind::ThreadInventory => change_request_thread_inventory::decode(arguments),
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
    /// A complete changed-file search did not contain the requested path.
    NotFound,
    /// Complete ownership evidence did not place the named review thread
    /// inside the named change request, so the mutation was never dispatched.
    ThreadNotInChangeRequest,
    /// The bounded response did not match the typed result contract.
    InvalidResponse,
    /// Response bytes exceeded the fixed operation cap.
    ResponseTooLarge,
    /// A multi-request read observed the change-request base or head move.
    ChangeRequestRevisionChanged,
    /// An infrastructure failure before a mutation existed as a request
    /// proved the mutation was never dispatched, so nothing was written.
    MutationNotDispatched,
    /// Transport loss left physical dispatch unknown.
    DispatchUnknown,
}

/// Mockable typed transport boundary for the code-host suite.
pub trait CodeHostTransport: Send {
    /// Returns the deployment policy used to construct results.
    fn numeric_bounds(&self) -> CodeHostNumericBounds;

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

/// All fourteen compiled code-host declarations and their matching executor.
///
/// Effect postures are explicit per declaration: summary, files, patch,
/// checks, threads, job-log, stack, and thread-inventory reads are `Auto`;
/// comment, reply, resolve, and rerun mutations
/// are `Confirm`. Every operation is `ExternalEffect` because even an
/// authenticated read is visible to the remote code host.
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
        let changed_file_not_found_detail =
            ToolExecutionErrorDetail::try_new(String::from(CHANGED_FILE_NOT_FOUND_DETAIL))
                .map_err(|_| CodeHostToolsConstructionError::ErrorDetail)?;
        let thread_not_in_change_request_detail =
            ToolExecutionErrorDetail::try_new(String::from(THREAD_NOT_IN_CHANGE_REQUEST_DETAIL))
                .map_err(|_| CodeHostToolsConstructionError::ErrorDetail)?;
        let mut compiled = Vec::with_capacity(CodeHostToolKind::ALL.len());
        for &kind in CodeHostToolKind::ALL {
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
                changed_file_not_found_detail,
                thread_not_in_change_request_detail,
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

/// Credential-resolving executor for all fourteen code-host declarations.
#[derive(Clone, Debug)]
pub struct CodeHostExecutor<Credentials, Transport> {
    credentials: Credentials,
    credential_reference: CredentialReference,
    transport: Transport,
    credential_unavailable_detail: ToolExecutionErrorDetail,
    code_host_rejected_detail: ToolExecutionErrorDetail,
    changed_file_not_found_detail: ToolExecutionErrorDetail,
    thread_not_in_change_request_detail: ToolExecutionErrorDetail,
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
        let result_operation = operation.clone();
        let result = match self.transport.execute(operation, &credential).await {
            Ok(result)
                if kind.accepts_result(&result)
                    && result_operation.accepts_repository_result(&result) =>
            {
                result
            }
            Ok(_) => return Err(caller_bug()),
            Err(failure) => {
                if let Some(class) = transport_failure_class(kind, failure) {
                    return Err(CodeHostExecutorError { class });
                }
                let Some(detail) = transport_failure_detail(failure) else {
                    return Err(caller_bug());
                };
                let detail = match detail {
                    KnownFailureDetail::CredentialUnavailable => {
                        self.credential_unavailable_detail.clone()
                    }
                    KnownFailureDetail::CodeHostRejected => self.code_host_rejected_detail.clone(),
                    KnownFailureDetail::ChangedFileNotFound => {
                        self.changed_file_not_found_detail.clone()
                    }
                    KnownFailureDetail::ThreadNotInChangeRequest => {
                        self.thread_not_in_change_request_detail.clone()
                    }
                };
                return Ok(invocation.bind(ToolExecutorEvidence::KnownFailed {
                    detail: Some(detail),
                }));
            }
        };
        let mut value = result.into_json_value();
        if scrub_result_value(self.transport.numeric_bounds(), kind, &scrubber, &mut value)
            .is_none()
        {
            return Ok(invocation.bind(ToolExecutorEvidence::KnownFailed {
                detail: Some(self.code_host_rejected_detail.clone()),
            }));
        }
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
    /// A complete changed-file search did not contain the requested path.
    ChangedFileNotFound,
    /// Complete ownership evidence did not place the named review thread
    /// inside the named change request.
    ThreadNotInChangeRequest,
}

/// Shapes a known-failure detail after its class is known to be non-fatal.
///
/// Credential bytes that cannot form the authentication header are a
/// deployment fact the operator can act on, so they read as a credential
/// failure rather than as something the code host decided; the code host never
/// saw the request.
const fn transport_failure_detail(failure: CodeHostTransportFailure) -> Option<KnownFailureDetail> {
    match failure {
        CodeHostTransportFailure::InvalidCredential => {
            Some(KnownFailureDetail::CredentialUnavailable)
        }
        CodeHostTransportFailure::NotFound => Some(KnownFailureDetail::ChangedFileNotFound),
        CodeHostTransportFailure::ThreadNotInChangeRequest => {
            Some(KnownFailureDetail::ThreadNotInChangeRequest)
        }
        CodeHostTransportFailure::Rejected
        | CodeHostTransportFailure::InvalidResponse
        | CodeHostTransportFailure::ResponseTooLarge => Some(KnownFailureDetail::CodeHostRejected),
        CodeHostTransportFailure::ChangeRequestRevisionChanged
        | CodeHostTransportFailure::MutationNotDispatched
        | CodeHostTransportFailure::DispatchUnknown => None,
    }
}

const fn transport_failure_class(
    kind: CodeHostToolKind,
    failure: CodeHostTransportFailure,
) -> Option<OperatorFailureClass> {
    match failure {
        CodeHostTransportFailure::InvalidCredential
        | CodeHostTransportFailure::Rejected
        | CodeHostTransportFailure::NotFound
        | CodeHostTransportFailure::ThreadNotInChangeRequest => None,
        CodeHostTransportFailure::InvalidResponse | CodeHostTransportFailure::ResponseTooLarge
            if !kind.mutates() =>
        {
            None
        }
        // The transport proved the mutation never existed as a request, so
        // the infrastructure failure carries no commit ambiguity even for a
        // mutating declaration.
        CodeHostTransportFailure::MutationNotDispatched => {
            Some(OperatorFailureClass::Infrastructure {
                commit_ambiguous: false,
            })
        }
        CodeHostTransportFailure::InvalidResponse
        | CodeHostTransportFailure::ResponseTooLarge
        | CodeHostTransportFailure::ChangeRequestRevisionChanged
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

fn scrub_result_value(
    bounds: CodeHostNumericBounds,
    kind: CodeHostToolKind,
    scrubber: &CredentialScrubber,
    value: &mut serde_json::Value,
) -> Option<()> {
    let repository_structure = repository_result_structure(kind, value);
    let is_file_content = kind == CodeHostToolKind::ReadFile
        && value.get("outcome").and_then(serde_json::Value::as_str) == Some("content");
    scrubber.redact_value(value);
    if repository_structure
        .as_ref()
        .is_some_and(|expected| repository_result_structure(kind, value).as_ref() != Some(expected))
    {
        return None;
    }
    if kind == CodeHostToolKind::ReviewThreads {
        result::bound_scrubbed_review_threads_value(bounds, value)?;
    }
    if !is_file_content {
        return Some(());
    }

    let content = value.get("content")?.as_str()?;
    let returned_bytes = content.len();
    if bounds
        .repository_file_content_bytes()
        .is_some_and(|limit| returned_bytes > limit)
    {
        return None;
    }
    let returned_lines = repository_result::line_count(content);
    if value.get("returned_lines")?.as_u64()? != u64::from(returned_lines) {
        return None;
    }
    let truncated = value.get("truncated")?.as_bool()?;
    let last_line_complete = content.is_empty() || content.ends_with('\n') || !truncated;
    if value.get("last_line_complete")?.as_bool()? != last_line_complete {
        return None;
    }
    value.as_object_mut()?.insert(
        String::from("returned_bytes"),
        serde_json::Value::from(returned_bytes),
    );
    Some(())
}

fn repository_result_structure(
    kind: CodeHostToolKind,
    value: &serde_json::Value,
) -> Option<serde_json::Value> {
    if kind == CodeHostToolKind::ListDirectory {
        return Some(value.clone());
    }
    if kind != CodeHostToolKind::ReadFile {
        return None;
    }

    let mut structure = value.clone();
    let object = structure.as_object_mut()?;
    if object.get("outcome").and_then(serde_json::Value::as_str) == Some("content") {
        object.remove("content")?;
        object.remove("returned_bytes")?;
    }
    Some(structure)
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

    #[test]
    fn configured_result_policies_distinguish_finite_and_unbounded_values() {
        const OBSERVED: usize = 5;
        let finite = CodeHostNumericBounds::new(
            None,
            None,
            None,
            Some(OBSERVED - 1),
            Some(OBSERVED - 1),
            None,
        );
        let unbounded = test_numeric_bounds();

        assert!(!finite.permits_result_text(OBSERVED));
        assert!(!finite.permits_result_items(OBSERVED));
        assert!(unbounded.permits_result_text(OBSERVED));
        assert!(unbounded.permits_result_items(OBSERVED));
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

    #[track_caller]
    fn assert_invalid(catalog: &CompiledToolCatalog, name: &str, value: &str) {
        let name = ToolName::try_new(name.to_owned()).expect("fixture name is admitted");
        assert!(
            catalog
                .validate_arguments(&name, &arguments(value))
                .is_err()
        );
    }

    /// The fourteen declarations encode the read/mutation approval split and make
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
            repository_list_directory::NAME,
            ToolPermissionDefault::Auto,
        );
        assert_definition(
            &catalog,
            repository_read_file::NAME,
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
            change_request_stack_state::NAME,
            ToolPermissionDefault::Auto,
        );
        assert_definition(
            &catalog,
            change_request_thread_inventory::NAME,
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
                      "pattern": "^(?:[^./\\u0000][^/\\u0000]*|\\.[^./\\u0000][^/\\u0000]*|\\.\\.[^/\\u0000]+)(?:/(?:[^./\\u0000][^/\\u0000]*|\\.[^./\\u0000][^/\\u0000]*|\\.\\.[^/\\u0000]+))*$",
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

    /// Complete model-facing schema for `repository_list_directory`.
    #[test]
    fn repository_list_directory_schema_is_the_exact_wire_artifact() {
        assert_schema(
            repository_list_directory::NAME,
            expect_test::expect![[r#"
                {
                  "additionalProperties": false,
                  "properties": {
                    "path": {
                      "description": "Exact repository-relative directory path; `.` lists the repository root.",
                      "maxLength": 4096,
                      "minLength": 1,
                      "pattern": "^(?:\\.|(?:[^./\\u0000][^/\\u0000]*|\\.[^./\\u0000][^/\\u0000]*|\\.\\.[^/\\u0000]+)(?:/(?:[^./\\u0000][^/\\u0000]*|\\.[^./\\u0000][^/\\u0000]*|\\.\\.[^/\\u0000]+))*)$",
                      "type": "string"
                    },
                    "repository": {
                      "description": "Exact owner/repository spelling.",
                      "maxLength": 256,
                      "pattern": "^(?:[A-Za-z0-9_-]|\\.[A-Za-z0-9_-]|\\.\\.[A-Za-z0-9._-])[A-Za-z0-9._-]*/(?:[A-Za-z0-9_-]|\\.[A-Za-z0-9_-]|\\.\\.[A-Za-z0-9._-])[A-Za-z0-9._-]*$",
                      "type": "string"
                    },
                    "revision": {
                      "description": "Required exact lowercase 40-hex commit revision.",
                      "maxLength": 40,
                      "minLength": 40,
                      "pattern": "^[0-9a-f]{40}$",
                      "type": "string"
                    }
                  },
                  "required": [
                    "repository",
                    "path",
                    "revision"
                  ],
                  "type": "object"
                }"#]],
        );
    }

    /// Complete model-facing schema for `repository_read_file`.
    #[test]
    fn repository_read_file_schema_is_the_exact_wire_artifact() {
        assert_schema(
            repository_read_file::NAME,
            expect_test::expect![[r##"
                {
                  "$defs": {
                    "RepositoryLineRange": {
                      "additionalProperties": false,
                      "description": "One inclusive, positive line range.",
                      "properties": {
                        "end": {
                          "description": "Last one-based line to return, inclusive.",
                          "maximum": 4294967295,
                          "minimum": 1,
                          "type": "integer"
                        },
                        "start": {
                          "description": "First one-based line to return.",
                          "maximum": 4294967295,
                          "minimum": 1,
                          "type": "integer"
                        }
                      },
                      "required": [
                        "start",
                        "end"
                      ],
                      "type": "object"
                    }
                  },
                  "additionalProperties": false,
                  "properties": {
                    "line_range": {
                      "anyOf": [
                        {
                          "$ref": "#/$defs/RepositoryLineRange"
                        },
                        {
                          "type": "null"
                        }
                      ],
                      "description": "Optional inclusive one-based line range."
                    },
                    "path": {
                      "description": "Exact repository-relative path; `.` addresses the repository root.",
                      "maxLength": 4096,
                      "minLength": 1,
                      "pattern": "^(?:\\.|(?:[^./\\u0000][^/\\u0000]*|\\.[^./\\u0000][^/\\u0000]*|\\.\\.[^/\\u0000]+)(?:/(?:[^./\\u0000][^/\\u0000]*|\\.[^./\\u0000][^/\\u0000]*|\\.\\.[^/\\u0000]+))*)$",
                      "type": "string"
                    },
                    "repository": {
                      "description": "Exact owner/repository spelling.",
                      "maxLength": 256,
                      "pattern": "^(?:[A-Za-z0-9_-]|\\.[A-Za-z0-9_-]|\\.\\.[A-Za-z0-9._-])[A-Za-z0-9._-]*/(?:[A-Za-z0-9_-]|\\.[A-Za-z0-9_-]|\\.\\.[A-Za-z0-9._-])[A-Za-z0-9._-]*$",
                      "type": "string"
                    },
                    "revision": {
                      "description": "Required exact lowercase 40-hex commit revision.",
                      "maxLength": 40,
                      "minLength": 40,
                      "pattern": "^[0-9a-f]{40}$",
                      "type": "string"
                    }
                  },
                  "required": [
                    "repository",
                    "path",
                    "revision"
                  ],
                  "type": "object"
                }"##]],
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
                    "number": {
                      "description": "Number of the owning change request.",
                      "maximum": 2147483647,
                      "minimum": 1,
                      "type": "integer"
                    },
                    "repository": {
                      "description": "Exact owner/repository spelling of the owning change request.",
                      "maxLength": 256,
                      "pattern": "^(?:[A-Za-z0-9_-]|\\.[A-Za-z0-9_-]|\\.\\.[A-Za-z0-9._-])[A-Za-z0-9._-]*/(?:[A-Za-z0-9_-]|\\.[A-Za-z0-9_-]|\\.\\.[A-Za-z0-9._-])[A-Za-z0-9._-]*$",
                      "type": "string"
                    },
                    "thread_id": {
                      "description": "Opaque review-thread node identity inside the named change request.",
                      "maxLength": 512,
                      "minLength": 1,
                      "pattern": "^[^\\u0000-\\u001F\\u007F-\\u009F]+$",
                      "type": "string"
                    }
                  },
                  "required": [
                    "repository",
                    "number",
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
                    "number": {
                      "description": "Number of the owning change request.",
                      "maximum": 2147483647,
                      "minimum": 1,
                      "type": "integer"
                    },
                    "repository": {
                      "description": "Exact owner/repository spelling of the owning change request.",
                      "maxLength": 256,
                      "pattern": "^(?:[A-Za-z0-9_-]|\\.[A-Za-z0-9_-]|\\.\\.[A-Za-z0-9._-])[A-Za-z0-9._-]*/(?:[A-Za-z0-9_-]|\\.[A-Za-z0-9_-]|\\.\\.[A-Za-z0-9._-])[A-Za-z0-9._-]*$",
                      "type": "string"
                    },
                    "thread_id": {
                      "description": "Opaque review-thread node identity inside the named change request.",
                      "maxLength": 512,
                      "minLength": 1,
                      "pattern": "^[^\\u0000-\\u001F\\u007F-\\u009F]+$",
                      "type": "string"
                    }
                  },
                  "required": [
                    "repository",
                    "number",
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

    /// Complete model-facing schema for `change_request_stack_state`.
    #[test]
    fn change_request_stack_state_schema_is_the_exact_wire_artifact() {
        assert_schema(
            change_request_stack_state::NAME,
            expect_test::expect![[r#"
                {
                  "additionalProperties": false,
                  "properties": {
                    "cursor": {
                      "description": "Optional opaque child-page continuation cursor.",
                      "maxLength": 512,
                      "minLength": 1,
                      "pattern": "^[^\\u0000-\\u001F\\u007F-\\u009F]+$",
                      "type": [
                        "string",
                        "null"
                      ]
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
                    "number"
                  ],
                  "type": "object"
                }"#]],
        );
    }

    /// Complete model-facing schema for `change_request_thread_inventory`.
    #[test]
    fn change_request_thread_inventory_schema_is_the_exact_wire_artifact() {
        assert_schema(
            change_request_thread_inventory::NAME,
            expect_test::expect![[r#"
                {
                  "additionalProperties": false,
                  "properties": {
                    "cursor": {
                      "description": "Optional opaque review-thread continuation cursor.",
                      "maxLength": 512,
                      "minLength": 1,
                      "pattern": "^[^\\u0000-\\u001F\\u007F-\\u009F]+$",
                      "type": [
                        "string",
                        "null"
                      ]
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
                    "number"
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
        let mut schema = schema.to_value();
        schema
            .as_object_mut()
            .expect("URL schema is an object")
            .sort_keys();

        expect_test::expect![[r#"
            {
              "format": "uri",
              "maxLength": 8192,
              "pattern": "^https://[^/@\\u0000-\\u0020\\u007F-\\u009F]+(?:[/?#]|$)",
              "type": "string"
            }"#]]
        .assert_eq(&format!("{schema:#}"));
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

    /// The repository root cannot identify one changed file.
    #[test]
    fn change_request_file_patch_typed_decode_rejects_bare_dot_root() {
        assert_invalid(
            &catalog(),
            change_request_file_patch::NAME,
            r#"{"number":17,"path":".","repository":"owner/repository"}"#,
        );
    }

    /// Directory-list arguments require an exact path and revision.
    #[test]
    fn repository_list_directory_typed_decode_accepts_exact_shape() {
        assert_valid(
            &catalog(),
            repository_list_directory::NAME,
            r#"{"path":"src","repository":"owner/repository","revision":"0123456789abcdef0123456789abcdef01234567"}"#,
        );
    }

    /// The documented bare-dot spelling selects the repository root.
    #[test]
    fn repository_list_directory_typed_decode_accepts_bare_dot_root() {
        assert_valid(
            &catalog(),
            repository_list_directory::NAME,
            r#"{"path":".","repository":"owner/repository","revision":"0123456789abcdef0123456789abcdef01234567"}"#,
        );
    }

    /// A trailing separator cannot turn an existing directory into false
    /// absence evidence after GitHub canonicalizes its response path.
    #[test]
    fn repository_list_directory_typed_decode_rejects_trailing_separator() {
        assert_invalid(
            &catalog(),
            repository_list_directory::NAME,
            r#"{"path":"src/","repository":"owner/repository","revision":"0123456789abcdef0123456789abcdef01234567"}"#,
        );
    }

    /// Empty interior components are not canonical repository path identities.
    #[test]
    fn repository_read_file_typed_decode_rejects_empty_path_component() {
        assert_invalid(
            &catalog(),
            repository_read_file::NAME,
            r#"{"path":"src//lib.rs","repository":"owner/repository","revision":"0123456789abcdef0123456789abcdef01234567"}"#,
        );
    }

    /// Dot components are not admitted as aliases for canonical paths.
    #[test]
    fn repository_read_file_typed_decode_rejects_dot_path_component() {
        assert_invalid(
            &catalog(),
            repository_read_file::NAME,
            r#"{"path":"./src/lib.rs","repository":"owner/repository","revision":"0123456789abcdef0123456789abcdef01234567"}"#,
        );
    }

    /// Parent components cannot select an alias whose echoed host path differs.
    #[test]
    fn repository_read_file_typed_decode_rejects_parent_path_component() {
        assert_invalid(
            &catalog(),
            repository_read_file::NAME,
            r#"{"path":"src/../lib.rs","repository":"owner/repository","revision":"0123456789abcdef0123456789abcdef01234567"}"#,
        );
    }

    /// Dot-prefixed names remain canonical components even though the aliases
    /// `.` and `..` are rejected away from the documented bare-dot root.
    #[test]
    fn repository_read_file_typed_decode_accepts_dot_prefixed_components() {
        assert_valid(
            &catalog(),
            repository_read_file::NAME,
            r#"{"path":".git/objects/...","repository":"owner/repository","revision":"0123456789abcdef0123456789abcdef01234567"}"#,
        );
    }

    /// File-read arguments retain one inclusive line range at an exact revision.
    #[test]
    fn repository_read_file_typed_decode_accepts_line_range() {
        assert_valid(
            &catalog(),
            repository_read_file::NAME,
            r#"{"line_range":{"end":40,"start":20},"path":"src/lib.rs","repository":"owner/repository","revision":"0123456789abcdef0123456789abcdef01234567"}"#,
        );
    }

    /// A repository file read cannot silently default to a moving branch head.
    #[test]
    fn repository_read_file_typed_decode_requires_revision() {
        assert_invalid(
            &catalog(),
            repository_read_file::NAME,
            r#"{"path":"src/lib.rs","repository":"owner/repository"}"#,
        );
    }

    /// A reversed range cannot become a misleading empty successful read.
    #[test]
    fn repository_read_file_typed_decode_rejects_reversed_range() {
        assert_invalid(
            &catalog(),
            repository_read_file::NAME,
            r#"{"line_range":{"end":20,"start":40},"path":"src/lib.rs","repository":"owner/repository","revision":"0123456789abcdef0123456789abcdef01234567"}"#,
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

    /// Thread-reply arguments retain the owning change request beside the
    /// opaque identity and bounded body.
    #[test]
    fn change_request_thread_reply_typed_decode_accepts_exact_shape() {
        assert_valid(
            &catalog(),
            change_request_thread_reply::NAME,
            r#"{"body":"fixed","number":17,"repository":"owner/repository","thread_id":"PRRT_node"}"#,
        );
    }

    /// A thread reply cannot omit the change request whose grant authorizes
    /// it, because execution validates thread ownership against that target.
    #[test]
    fn change_request_thread_reply_typed_decode_requires_the_change_request() {
        assert_invalid(
            &catalog(),
            change_request_thread_reply::NAME,
            r#"{"body":"fixed","thread_id":"PRRT_node"}"#,
        );
    }

    /// Thread-resolve arguments retain the owning change request beside one
    /// opaque node identity.
    #[test]
    fn change_request_thread_resolve_typed_decode_accepts_exact_shape() {
        assert_valid(
            &catalog(),
            change_request_thread_resolve::NAME,
            r#"{"number":17,"repository":"owner/repository","thread_id":"PRRT_node"}"#,
        );
    }

    /// A thread resolution cannot omit the change request whose grant
    /// authorizes it, because execution validates thread ownership against
    /// that target.
    #[test]
    fn change_request_thread_resolve_typed_decode_requires_the_change_request() {
        assert_invalid(
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

    /// Stack-state arguments retain the optional opaque child-page cursor.
    #[test]
    fn change_request_stack_state_typed_decode_accepts_cursor() {
        assert_valid(
            &catalog(),
            change_request_stack_state::NAME,
            r#"{"cursor":"opaque-child-page","number":17,"repository":"owner/repository"}"#,
        );
    }

    /// Thread-inventory arguments retain the optional opaque GraphQL cursor.
    #[test]
    fn change_request_thread_inventory_typed_decode_accepts_cursor() {
        assert_valid(
            &catalog(),
            change_request_thread_inventory::NAME,
            r#"{"cursor":"opaque-page","number":17,"repository":"owner/repository"}"#,
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

    /// The aggregate review-thread ceiling applies after credential redaction
    /// expands the exact model-visible result.
    #[test]
    fn review_thread_result_is_rebounded_after_credential_scrubbing() {
        const CREDENTIAL: &str = "!";
        const REPETITIONS: usize = 64;
        let credential = CredentialValue::new(CREDENTIAL.as_bytes().to_vec());
        let scrubber =
            CredentialScrubber::try_new(&credential).expect("fixture credential is usable");
        let credential_body = CREDENTIAL.repeat(REPETITIONS);
        let redacted_body = "[redacted]".repeat(REPETITIONS);
        let mut value = serde_json::json!({
            "threads": [{
                "comments": [
                    {
                        "author": "reviewer",
                        "body": credential_body,
                        "id": "PRRC_first",
                        "url": "https://host.invalid/comments/first",
                    },
                    {
                        "author": "reviewer",
                        "body": credential_body,
                        "id": "PRRC_second",
                        "url": "https://host.invalid/comments/second",
                    },
                ],
                "comments_truncated": false,
                "id": "PRRT_first",
                "line": 17,
                "outdated": false,
                "path": "src/lib.rs",
                "resolved": false,
            }],
            "truncated": false,
        });
        let expected = serde_json::json!({
            "threads": [{
                "comments": [{
                    "author": "reviewer",
                    "body": redacted_body,
                    "id": "PRRC_first",
                    "url": "https://host.invalid/comments/first",
                }],
                "comments_truncated": true,
                "id": "PRRT_first",
                "line": 17,
                "outdated": false,
                "path": "src/lib.rs",
                "resolved": false,
            }],
            "truncated": false,
        });
        let encoded_limit = serde_json::to_vec(&expected)
            .expect("fixture expected result encodes")
            .len();
        let bounds = CodeHostNumericBounds::new(None, None, None, Some(encoded_limit), None, None);

        assert!(
            serde_json::to_vec(&value)
                .expect("fixture source result encodes")
                .len()
                <= encoded_limit
        );
        scrub_result_value(
            bounds,
            CodeHostToolKind::ReviewThreads,
            &scrubber,
            &mut value,
        )
        .expect("scrubbed review-thread prefix remains representable");

        assert_eq!(value, expected);
        assert!(
            serde_json::to_vec(&value)
                .expect("fixture scrubbed result encodes")
                .len()
                <= encoded_limit
        );
        assert!(!value.to_string().contains(CREDENTIAL));
    }

    /// File byte counts describe emitted scrubbed content rather than the
    /// credential-bearing source retained before evidence redaction.
    #[test]
    fn repository_file_returned_bytes_are_counted_after_credential_scrubbing() {
        const CREDENTIAL: &str = "github_pat_synthetic_1234567890abcdefghijklmnopqrstuvwxyz";
        const REVISION: &str = "0123456789abcdef0123456789abcdef01234567";
        let credential = CredentialValue::new(CREDENTIAL.as_bytes().to_vec());
        let scrubber =
            CredentialScrubber::try_new(&credential).expect("fixture credential is usable");
        let source_content = format!("before {CREDENTIAL} after\n");
        let source_bytes =
            u64::try_from(source_content.len()).expect("fixture source size fits u64");
        let arguments: RepositoryReadFileArguments = serde_json::from_value(serde_json::json!({
            "path": "src/lib.rs",
            "repository": "owner/repository",
            "revision": REVISION,
        }))
        .expect("fixture file arguments are admitted");
        let result = RepositoryReadFileResult::try_content(
            crate::code_host::test_numeric_bounds(),
            &arguments,
            RepositoryFileContentFields {
                source_bytes,
                selected_source_bytes: source_bytes,
                start_line: Some(1),
                end_line: Some(1),
                returned_lines: 1,
                last_line_complete: true,
                content: source_content,
                completeness: CodeHostResultCompleteness::Complete,
            },
        )
        .expect("fixture file result is admitted");
        let mut value = CodeHostResult::ReadFile(result).into_json_value();

        scrub_result_value(
            test_numeric_bounds(),
            CodeHostToolKind::ReadFile,
            &scrubber,
            &mut value,
        )
        .expect("typed file result remains shaped after scrubbing");

        let emitted_content = "before [redacted] after\n";
        assert_eq!(value["content"], emitted_content);
        assert_eq!(value["returned_bytes"], emitted_content.len());
        assert_eq!(value["source_bytes"], source_bytes);
        assert!(!value.to_string().contains(CREDENTIAL));
    }

    /// Credential redaction cannot expand emitted file content past its
    /// advertised retained-content bound.
    #[test]
    fn repository_file_scrubbing_rejects_content_expansion_past_the_bound() {
        const CREDENTIAL: &str = "content";
        const SOURCE_CONTENT_REPETITIONS: usize = 8 * 1024;
        const REVISION: &str = "0123456789abcdef0123456789abcdef01234567";
        let credential = CredentialValue::new(CREDENTIAL.as_bytes().to_vec());
        let scrubber =
            CredentialScrubber::try_new(&credential).expect("fixture credential is usable");
        let source_content = CREDENTIAL.repeat(SOURCE_CONTENT_REPETITIONS);
        let bounds =
            CodeHostNumericBounds::new(None, None, None, None, None, Some(source_content.len()));
        let source_bytes =
            u64::try_from(source_content.len()).expect("fixture source size fits u64");
        let arguments: RepositoryReadFileArguments = serde_json::from_value(serde_json::json!({
            "path": "src/lib.rs",
            "repository": "owner/repository",
            "revision": REVISION,
        }))
        .expect("fixture file arguments are admitted");
        let result = RepositoryReadFileResult::try_content(
            bounds,
            &arguments,
            RepositoryFileContentFields {
                source_bytes,
                selected_source_bytes: source_bytes,
                start_line: Some(1),
                end_line: Some(1),
                returned_lines: 1,
                last_line_complete: true,
                content: source_content,
                completeness: CodeHostResultCompleteness::Complete,
            },
        )
        .expect("fixture file result is admitted");
        let mut value = CodeHostResult::ReadFile(result).into_json_value();

        let scrubbed =
            scrub_result_value(bounds, CodeHostToolKind::ReadFile, &scrubber, &mut value);

        assert!(scrubbed.is_none());
    }

    /// Credential redaction cannot leave line metadata describing the
    /// credential-bearing source rather than the emitted content.
    #[test]
    fn repository_file_scrubbing_rejects_changed_line_structure() {
        const CREDENTIAL: &str = "\nsecond";
        const REVISION: &str = "0123456789abcdef0123456789abcdef01234567";
        const SOURCE_CONTENT: &str = "first\nsecond\nthird\n";
        let credential = CredentialValue::new(CREDENTIAL.as_bytes().to_vec());
        let scrubber =
            CredentialScrubber::try_new(&credential).expect("fixture credential is usable");
        let source_bytes =
            u64::try_from(SOURCE_CONTENT.len()).expect("fixture source size fits u64");
        let arguments: RepositoryReadFileArguments = serde_json::from_value(serde_json::json!({
            "path": "src/lib.rs",
            "repository": "owner/repository",
            "revision": REVISION,
        }))
        .expect("fixture file arguments are admitted");
        let result = RepositoryReadFileResult::try_content(
            crate::code_host::test_numeric_bounds(),
            &arguments,
            RepositoryFileContentFields {
                source_bytes,
                selected_source_bytes: source_bytes,
                start_line: Some(1),
                end_line: Some(3),
                returned_lines: 3,
                last_line_complete: true,
                content: String::from(SOURCE_CONTENT),
                completeness: CodeHostResultCompleteness::Complete,
            },
        )
        .expect("fixture file result is admitted before scrubbing");
        let mut value = CodeHostResult::ReadFile(result).into_json_value();

        let scrubbed = scrub_result_value(
            test_numeric_bounds(),
            CodeHostToolKind::ReadFile,
            &scrubber,
            &mut value,
        );

        assert!(scrubbed.is_none());
    }

    /// Credential redaction cannot collapse distinct directory entry
    /// identities after the typed result validates their uniqueness.
    #[test]
    fn repository_directory_scrubbing_rejects_entry_identity_collision() {
        const CREDENTIAL: &str = "token";
        const REVISION: &str = "0123456789abcdef0123456789abcdef01234567";
        let credential = CredentialValue::new(CREDENTIAL.as_bytes().to_vec());
        let scrubber =
            CredentialScrubber::try_new(&credential).expect("fixture credential is usable");
        let arguments: RepositoryListDirectoryArguments =
            serde_json::from_value(serde_json::json!({
                "path": "src",
                "repository": "owner/repository",
                "revision": REVISION,
            }))
            .expect("fixture directory arguments are admitted");
        let credential_path = format!("{}/{CREDENTIAL}", arguments.path().as_str());
        let redacted_path = format!("{}/[redacted]", arguments.path().as_str());
        let entries = vec![
            RepositoryDirectoryEntry::try_new(credential_path, RepositoryObjectKind::File, Some(1))
                .expect("fixture credential-bearing entry is admitted"),
            RepositoryDirectoryEntry::try_new(redacted_path, RepositoryObjectKind::File, Some(2))
                .expect("fixture redacted-spelling entry is admitted"),
        ];
        let observed_entries = entries.len();
        let result = RepositoryListDirectoryResult::try_entries(
            crate::code_host::test_numeric_bounds(),
            &arguments,
            entries,
            observed_entries,
            CodeHostResultCompleteness::Complete,
        )
        .expect("fixture directory result is admitted before scrubbing");
        let mut value = CodeHostResult::ListDirectory(result).into_json_value();

        let scrubbed = scrub_result_value(
            test_numeric_bounds(),
            CodeHostToolKind::ListDirectory,
            &scrubber,
            &mut value,
        );

        assert!(scrubbed.is_none());
    }

    /// A custom transport cannot satisfy one exact repository operation with a
    /// valid result constructed for another path and revision.
    #[test]
    fn repository_operation_rejects_another_result_identity() {
        let requested: RepositoryReadFileArguments = serde_json::from_value(serde_json::json!({
            "path": "src/a.rs",
            "repository": "owner/repository",
            "revision": "0123456789abcdef0123456789abcdef01234567",
        }))
        .expect("fixture requested arguments are admitted");
        let returned: RepositoryReadFileArguments = serde_json::from_value(serde_json::json!({
            "path": "src/b.rs",
            "repository": "owner/repository",
            "revision": "89abcdef0123456789abcdef0123456789abcdef",
        }))
        .expect("fixture returned arguments are admitted");
        let result = CodeHostResult::ReadFile(
            RepositoryReadFileResult::try_path_not_found(&returned)
                .expect("fixture non-root path admits absence"),
        );
        let operation = CodeHostOperation::ReadFile(requested);

        assert!(!operation.accepts_repository_result(&result));
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

    /// An ownership refusal is the code host's definitive answer before any
    /// mutation was dispatched, so it stays a model-visible known failure
    /// rather than an operator failure.
    #[test]
    fn thread_ownership_refusal_is_a_known_failure() {
        assert_eq!(
            transport_failure_class(
                CodeHostToolKind::ThreadResolve,
                CodeHostTransportFailure::ThreadNotInChangeRequest,
            ),
            None
        );
        assert_eq!(
            transport_failure_detail(CodeHostTransportFailure::ThreadNotInChangeRequest),
            Some(KnownFailureDetail::ThreadNotInChangeRequest)
        );
    }

    /// A failure the transport proved happened before the mutation existed as
    /// a request carries no commit ambiguity even for a mutating declaration.
    #[test]
    fn undispatched_mutation_failure_is_not_commit_ambiguous() {
        assert_eq!(
            transport_failure_class(
                CodeHostToolKind::ThreadReply,
                CodeHostTransportFailure::MutationNotDispatched,
            ),
            Some(OperatorFailureClass::Infrastructure {
                commit_ambiguous: false
            })
        );
    }

    /// Credential bytes that cannot form the authentication header never reach
    /// the code host, so the detail names the credential rather than blaming a
    /// decision the code host never made.
    #[test]
    fn invalid_credential_presents_the_credential_detail() {
        assert_eq!(
            transport_failure_detail(CodeHostTransportFailure::InvalidCredential),
            Some(KnownFailureDetail::CredentialUnavailable)
        );
    }

    /// A definitive answer from the code host keeps the rejection detail, so
    /// the credential detail stays a credential claim.
    #[test]
    fn rejected_request_presents_the_code_host_detail() {
        assert_eq!(
            transport_failure_detail(CodeHostTransportFailure::Rejected),
            Some(KnownFailureDetail::CodeHostRejected)
        );
    }

    /// A complete changed-file search miss reports its actual semantic result
    /// rather than claiming the host rejected a valid request.
    #[test]
    fn missing_changed_file_presents_the_not_found_detail() {
        assert_eq!(
            transport_failure_detail(CodeHostTransportFailure::NotFound),
            Some(KnownFailureDetail::ChangedFileNotFound)
        );
    }

    /// A bounded response the typed contract refuses is the code host's
    /// answer, not a credential fact.
    #[test]
    fn invalid_response_presents_the_code_host_detail() {
        assert_eq!(
            transport_failure_detail(CodeHostTransportFailure::InvalidResponse),
            Some(KnownFailureDetail::CodeHostRejected)
        );
    }

    /// Infrastructure failures are classified before known-failure detail is
    /// read, so neither can accidentally inherit a code-host rejection claim.
    #[test]
    fn infrastructure_failures_present_no_known_failure_detail() {
        assert_eq!(
            transport_failure_detail(CodeHostTransportFailure::ChangeRequestRevisionChanged),
            None
        );
        assert_eq!(
            transport_failure_detail(CodeHostTransportFailure::DispatchUnknown),
            None
        );
    }

    /// A returned node identity the argument side would refuse never enters
    /// durable evidence, so every identity a result offers can be passed back.
    #[test]
    fn returned_node_ids_reject_identities_the_argument_side_refuses() {
        let oversized = "N".repeat(arguments::MAX_OPAQUE_ID_BYTES + 1);

        let comment = ReviewThreadComment::try_new(
            crate::code_host::test_numeric_bounds(),
            oversized.clone(),
            Some(String::from("reviewer")),
            String::from("please adjust"),
            String::from("https://github.example/comment/7001"),
        );
        let thread = ReviewThread::try_new(
            crate::code_host::test_numeric_bounds(),
            ReviewThreadFields {
                id: oversized.clone(),
                resolved: false,
                outdated: false,
                path: String::from("src/lib.rs"),
                line: Some(12),
                comments: Vec::new(),
                comments_truncated: false,
            },
        );
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

        let summary = ChangeRequestSummaryResult::try_new(
            crate::code_host::test_numeric_bounds(),
            ChangeRequestSummaryFields {
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
            },
        );

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
            ThreadResolveResult::try_new(
                crate::code_host::test_numeric_bounds(),
                String::from("PRRT_node"),
                ReviewThreadResolution::Open,
            ),
            None
        );
    }
}
