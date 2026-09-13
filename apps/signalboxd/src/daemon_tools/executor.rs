#[cfg(doc)]
use super::catalog::DaemonToolCatalog;
use super::{
    pinned_file_system::PinFurtherWorkspaceRoot, workspace_executors::SessionWorkspaceExecutors,
};
use crate::{
    blob_tools::{BLOB_METADATA_NAME, BLOB_READ_NAME, BlobToolExecutor},
    goal_mode::{GOAL_DECLARE_NAME, GoalDeclarationExecutor},
    session_delegation::DaemonSessionDelegationPort,
};
use signalbox_application::{
    ClassifyOperatorFailure, CorrelatedDurableChildWait, CorrelatedToolExecutorEvidence,
    OperatorFailureClass, ToolExecutionInvocation, ToolExecutor, ToolExecutorDisposition,
};
use signalbox_model_runtime::CredentialAccess;
use signalbox_tools_basic::{
    CURRENT_TIME_NAME, CurrentTimeClock, CurrentTimeExecutor, ECHO_NAME, EchoExecutor,
    SESSION_STATUS_UPDATE_NAME, SessionStatusExecutor, SessionStatusWriter,
};
use signalbox_tools_code_host::{CODE_HOST_TOOL_NAMES, CodeHostExecutor, CodeHostTransport};
use signalbox_tools_conversations::{
    CONVERSATION_TOOL_NAMES, ConversationExecutor, ConversationIntrospectionPort,
};
use signalbox_tools_exec::{
    CARGO_DIAGNOSTICS_NAME, ProcessRunner, SANDBOXED_EXEC_NAME, UNSANDBOXED_EXEC_NAME,
};
use signalbox_tools_git::LOCAL_GIT_TOOL_NAMES;
use signalbox_tools_github::{GITHUB_TOOL_NAMES, GitHubExecutor, GitHubTransport};
use signalbox_tools_plan::{PLAN_TOOL_NAMES, PlanExecutor, SessionPlanPort};
use signalbox_tools_sessions::{
    SESSION_DELEGATION_TOOL_NAMES, SessionDelegationExecutionDisposition, SessionDelegationExecutor,
};
use signalbox_tools_web::{
    WEB_FETCH_NAME, WEB_SEARCH_NAME, WebFetchExecutor, WebFetchTransport, WebSearchExecutor,
    WebSearchTransport,
};
use signalbox_tools_workspace::{
    WORKSPACE_MUTATION_TOOL_NAMES, WORKSPACE_READ_TOOL_NAMES, WorkspaceFileSystem,
    WorkspaceMutationFileSystem,
};
use std::{error::Error, fmt};

/// Name-directed daemon executor matching [`DaemonToolCatalog`].
#[derive(Clone, Debug)]
pub struct DaemonToolExecutor<
    Clock,
    Transport,
    SearchTransport,
    Writer,
    Credentials,
    HostTransport,
    GitHubTransportType,
    FileSystem: WorkspaceMutationFileSystem,
    ConversationPort,
    PlanPort,
    ExecRunner: ProcessRunner,
> {
    pub(super) current_time: CurrentTimeExecutor<Clock>,
    pub(super) echo: EchoExecutor,
    pub(super) web_fetch: WebFetchExecutor<Transport>,
    pub(super) web_search: WebSearchExecutor<Credentials, SearchTransport>,
    pub(super) session_status: SessionStatusExecutor<Writer>,
    pub(super) code_host: CodeHostExecutor<Credentials, HostTransport>,
    pub(super) github: Option<GitHubExecutor<Credentials, GitHubTransportType>>,
    pub(super) workspace_bound: Option<SessionWorkspaceExecutors<FileSystem, ExecRunner>>,
    pub(super) conversations: Option<ConversationExecutor<ConversationPort>>,
    pub(super) plan: PlanExecutor<PlanPort>,
    pub(super) delegation: SessionDelegationExecutor<DaemonSessionDelegationPort>,
    pub(super) goal: Option<GoalDeclarationExecutor>,
    pub(super) blob: Option<BlobToolExecutor>,
    pub(super) workflows:
        Option<signalbox_tools_workflows::WorkflowExecutor<super::workflows::DaemonWorkflowPort>>,
    pub(super) file_media: Option<super::DaemonFileMediaExecutor>,
}

impl<
    Clock,
    Transport,
    SearchTransport,
    Writer,
    Credentials,
    HostTransport,
    GitHubTransportType,
    FileSystem,
    ConversationPort,
    PlanPort,
    ExecRunner,
>
    DaemonToolExecutor<
        Clock,
        Transport,
        SearchTransport,
        Writer,
        Credentials,
        HostTransport,
        GitHubTransportType,
        FileSystem,
        ConversationPort,
        PlanPort,
        ExecRunner,
    >
where
    FileSystem: WorkspaceMutationFileSystem,
    ExecRunner: ProcessRunner,
{
    /// Installs the workflow executor matching the compiled declarations.
    pub fn with_workflows(mut self, port: super::workflows::DaemonWorkflowPort) -> Self {
        self.workflows = Some(signalbox_tools_workflows::WorkflowExecutor(port));
        self
    }

    /// Supplies current repository configuration and persisted dispatch authority for pushes.
    pub fn with_repository_watch(
        mut self,
        watch: Option<crate::repo_watch_runtime::RepositoryWatchRuntime>,
    ) -> Self {
        if let Some(workspaces) = self.workspace_bound.as_mut() {
            workspaces.repository_watch = watch;
        }
        self
    }

    /// Supplies the live credential catalog for judged sandboxed tasks.
    pub fn with_ambient_credentials(
        mut self,
        catalogs: crate::configuration_reload::ConfigurationReload,
        pool: sqlx::PgPool,
    ) -> Self {
        if let Some(workspaces) = self.workspace_bound.as_mut() {
            workspaces.ambient_credentials = Some((catalogs, pool));
        }
        self
    }

    /// Installs the executor whose resolver and worker composed the file declarations.
    pub fn with_file_media_executor(
        mut self,
        executor: Option<super::DaemonFileMediaExecutor>,
    ) -> Self {
        self.file_media = executor;
        self
    }

    /// Installs the executor matching the composed blob-read declarations.
    ///
    /// An absent executor is the unconfigured deployment, whose catalog never
    /// received the declarations either.
    pub fn with_blob_executor(mut self, executor: Option<BlobToolExecutor>) -> Self {
        self.blob = executor;
        self
    }
}

/// Sanitized aggregate executor failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DaemonToolExecutorError {
    class: OperatorFailureClass,
}

impl DaemonToolExecutorError {
    pub(super) fn from_error(error: &impl ClassifyOperatorFailure) -> Self {
        Self {
            class: error.operator_failure_class(),
        }
    }

    pub(super) const fn pre_dispatch() -> Self {
        Self {
            class: OperatorFailureClass::Infrastructure {
                commit_ambiguous: false,
            },
        }
    }

    pub(super) const fn unknown_tool() -> Self {
        Self {
            class: OperatorFailureClass::CallerOrHubBug,
        }
    }
}

impl fmt::Display for DaemonToolExecutorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("daemon tool executor failed")
    }
}

impl Error for DaemonToolExecutorError {}

impl ClassifyOperatorFailure for DaemonToolExecutorError {
    fn operator_failure_class(&self) -> OperatorFailureClass {
        self.class
    }
}

impl<
    Clock,
    Transport,
    SearchTransport,
    Writer,
    Credentials,
    HostTransport,
    GitHubTransportType,
    FileSystem,
    ConversationPort,
    PlanPort,
    ExecRunner,
> ToolExecutor
    for DaemonToolExecutor<
        Clock,
        Transport,
        SearchTransport,
        Writer,
        Credentials,
        HostTransport,
        GitHubTransportType,
        FileSystem,
        ConversationPort,
        PlanPort,
        ExecRunner,
    >
where
    Clock: CurrentTimeClock,
    Transport: WebFetchTransport,
    SearchTransport: WebSearchTransport,
    Writer: SessionStatusWriter,
    Credentials: CredentialAccess,
    HostTransport: CodeHostTransport,
    GitHubTransportType: GitHubTransport,
    FileSystem: WorkspaceFileSystem + WorkspaceMutationFileSystem + PinFurtherWorkspaceRoot,
    ConversationPort: ConversationIntrospectionPort,
    PlanPort: SessionPlanPort,
    ExecRunner: ProcessRunner,
{
    type Error = DaemonToolExecutorError;

    async fn execute(
        &mut self,
        invocation: ToolExecutionInvocation,
    ) -> Result<CorrelatedToolExecutorEvidence, Self::Error> {
        let name = invocation.request().name().as_str();
        match name {
            name if signalbox_tools_workflows::WORKFLOW_TOOL_NAMES.contains(&name) => self
                .workflows
                .as_mut()
                .ok_or_else(DaemonToolExecutorError::unknown_tool)?
                .execute(invocation)
                .await
                .map_err(|error| DaemonToolExecutorError::from_error(&error)),
            CURRENT_TIME_NAME => self
                .current_time
                .execute(invocation)
                .await
                .map_err(|error| DaemonToolExecutorError::from_error(&error)),
            ECHO_NAME => self
                .echo
                .execute(invocation)
                .await
                .map_err(|error| DaemonToolExecutorError::from_error(&error)),
            WEB_FETCH_NAME => self
                .web_fetch
                .execute(invocation)
                .await
                .map_err(|error| DaemonToolExecutorError::from_error(&error)),
            WEB_SEARCH_NAME => self
                .web_search
                .execute(invocation)
                .await
                .map_err(|error| DaemonToolExecutorError::from_error(&error)),
            SESSION_STATUS_UPDATE_NAME => self
                .session_status
                .execute(invocation)
                .await
                .map_err(|error| DaemonToolExecutorError::from_error(&error)),
            name if CODE_HOST_TOOL_NAMES.contains(&name) => self
                .code_host
                .execute(invocation)
                .await
                .map_err(|error| DaemonToolExecutorError::from_error(&error)),
            name if GITHUB_TOOL_NAMES.contains(&name) => self
                .github
                .as_mut()
                .ok_or_else(DaemonToolExecutorError::unknown_tool)?
                .execute(invocation)
                .await
                .map_err(|error| DaemonToolExecutorError::from_error(&error)),
            name if WORKSPACE_READ_TOOL_NAMES.contains(&name)
                || WORKSPACE_MUTATION_TOOL_NAMES.contains(&name)
                || LOCAL_GIT_TOOL_NAMES.contains(&name)
                || name == signalbox_tools_git::GIT_PUSH_CONFIGURED_NAME
                || matches!(
                    name,
                    SANDBOXED_EXEC_NAME | UNSANDBOXED_EXEC_NAME | CARGO_DIAGNOSTICS_NAME
                ) =>
            {
                self.workspace_bound
                    .as_mut()
                    .ok_or_else(DaemonToolExecutorError::unknown_tool)?
                    .execute(invocation)
                    .await
            }
            name if CONVERSATION_TOOL_NAMES.contains(&name) => self
                .conversations
                .as_mut()
                .ok_or_else(DaemonToolExecutorError::unknown_tool)?
                .execute(invocation)
                .await
                .map_err(|error| DaemonToolExecutorError::from_error(&error)),
            name if SESSION_DELEGATION_TOOL_NAMES.contains(&name) => match self
                .delegation
                .execute_nonblocking(invocation)
                .await
                .map_err(|error| DaemonToolExecutorError::from_error(&error))?
            {
                SessionDelegationExecutionDisposition::Completed(evidence) => Ok(evidence),
                SessionDelegationExecutionDisposition::DurableCompletion(_)
                | SessionDelegationExecutionDisposition::ForegroundDelivered(_)
                | SessionDelegationExecutionDisposition::ForegroundPending(_) => {
                    Err(DaemonToolExecutorError::unknown_tool())
                }
            },
            GOAL_DECLARE_NAME => self
                .goal
                .as_mut()
                .ok_or_else(DaemonToolExecutorError::unknown_tool)?
                .execute(invocation)
                .await
                .map_err(|error| DaemonToolExecutorError::from_error(&error)),
            name if PLAN_TOOL_NAMES.contains(&name) => self
                .plan
                .execute(invocation)
                .await
                .map_err(|error| DaemonToolExecutorError::from_error(&error)),
            signalbox_tools_file_media::FILE_INSPECT_NAME
            | signalbox_tools_file_media::FILE_READ_NAME => {
                self.file_media
                    .as_mut()
                    .ok_or_else(Self::Error::unknown_tool)?
                    .execute(invocation)
                    .await
            }
            BLOB_METADATA_NAME | BLOB_READ_NAME => self
                .blob
                .as_mut()
                .ok_or_else(DaemonToolExecutorError::unknown_tool)?
                .execute(invocation)
                .await
                .map_err(|error| DaemonToolExecutorError::from_error(&error)),
            _ => Err(DaemonToolExecutorError::unknown_tool()),
        }
    }

    async fn execute_with_scheduling(
        &mut self,
        invocation: ToolExecutionInvocation,
    ) -> Result<ToolExecutorDisposition, Self::Error> {
        if SESSION_DELEGATION_TOOL_NAMES.contains(&invocation.request().name().as_str()) {
            return match self
                .delegation
                .execute_nonblocking(invocation)
                .await
                .map_err(|error| DaemonToolExecutorError::from_error(&error))?
            {
                SessionDelegationExecutionDisposition::Completed(evidence) => {
                    Ok(ToolExecutorDisposition::Completed(evidence))
                }
                SessionDelegationExecutionDisposition::DurableCompletion(evidence) => {
                    Ok(ToolExecutorDisposition::DurableCompletion(evidence))
                }
                SessionDelegationExecutionDisposition::ForegroundDelivered(delivered) => {
                    CorrelatedDurableChildWait::try_new(
                        delivered.correlation(),
                        delivered.result().wait(),
                    )
                    .map(ToolExecutorDisposition::DurableChildWait)
                    .ok_or_else(DaemonToolExecutorError::unknown_tool)
                }
                SessionDelegationExecutionDisposition::ForegroundPending(pending) => {
                    CorrelatedDurableChildWait::try_new(pending.correlation(), pending.wait())
                        .map(ToolExecutorDisposition::DurableChildWait)
                        .ok_or_else(DaemonToolExecutorError::unknown_tool)
                }
            };
        }
        self.execute(invocation)
            .await
            .map(ToolExecutorDisposition::Completed)
    }
}
