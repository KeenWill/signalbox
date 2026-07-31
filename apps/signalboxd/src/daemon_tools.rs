//! Process-lifetime compiled daemon tool catalog and executor dispatch.

use std::{collections::BTreeMap, error::Error, fmt, path::Path, sync::Arc};

use signalbox_application::{
    ClassifyOperatorFailure, CompiledToolCatalog, CorrelatedToolExecutorEvidence,
    OperatorFailureClass, ToolCatalog, ToolCatalogValidationFailure, ToolDefinition,
    ToolExecutionInvocation, ToolExecutor,
};
use signalbox_domain::{NormalizedToolArguments, ToolName};
use signalbox_model_runtime::CredentialAccess;
use signalbox_tools_basic::{
    CURRENT_TIME_NAME, CurrentTimeClock, CurrentTimeExecutor, CurrentTimeTool, ECHO_NAME,
    EchoExecutor, EchoTool, PostgresSessionStatusWriter, ReqwestWebFetchTransport,
    SESSION_STATUS_UPDATE_NAME, SessionStatusExecutor, SessionStatusTool, SessionStatusWriter,
    WEB_FETCH_NAME, WebFetchEgressPolicy, WebFetchExecutor, WebFetchTool, WebFetchTransport,
};
use signalbox_tools_code_host::{
    CODE_HOST_TOOL_NAMES, CodeHostExecutor, CodeHostTools, CodeHostTransport,
    GitHubCodeHostTransport,
};
use signalbox_tools_conversations::{
    CONVERSATION_TOOL_NAMES, ConversationExecutor, ConversationIntrospectionPort, ConversationTools,
};
use signalbox_tools_github::{
    GITHUB_TOOL_NAMES, GitHubApiTransport, GitHubEgressPolicy, GitHubExecutor, GitHubTools,
    GitHubTransport,
};
use signalbox_tools_workspace::{
    LocalWorkspaceFileSystem, WORKSPACE_MUTATION_TOOL_NAMES, WORKSPACE_READ_TOOL_NAMES,
    WorkspaceFileSystem, WorkspaceMutationExecutor, WorkspaceMutationFileSystem,
    WorkspaceMutationTools, WorkspaceReadExecutor, WorkspaceReadTools,
};
use sqlx::PgPool;
use tokio::sync::Mutex;

use crate::{FileCredentialAccess, PostgresConversationIntrospection};

struct ComposedToolFamilies<
    Transport,
    Writer,
    Credentials,
    HostTransport,
    GitHubTransportType,
    FileSystem: WorkspaceMutationFileSystem,
    Port,
> {
    web_fetch: WebFetchTool<Transport>,
    status: SessionStatusTool<Writer>,
    code_host: CodeHostTools<Credentials, HostTransport>,
    github: GitHubTools<Credentials, GitHubTransportType>,
    workspace_read: WorkspaceReadTools<FileSystem>,
    workspace_mutation: WorkspaceMutationTools<FileSystem>,
    conversations: ConversationTools<Port>,
}

/// The complete daemon-local declarations and their matching dispatch executor.
pub struct DaemonTools<
    Clock,
    Transport,
    Writer,
    Credentials,
    HostTransport,
    GitHubTransportType,
    FileSystem: WorkspaceMutationFileSystem,
    Port,
> {
    catalog: DaemonToolCatalog,
    executor: DaemonToolExecutor<
        Clock,
        Transport,
        Writer,
        Credentials,
        HostTransport,
        GitHubTransportType,
        FileSystem,
        Port,
    >,
}

impl<Clock>
    DaemonTools<
        Clock,
        ReqwestWebFetchTransport,
        PostgresSessionStatusWriter,
        FileCredentialAccess,
        GitHubCodeHostTransport,
        GitHubApiTransport,
        LocalWorkspaceFileSystem,
        PostgresConversationIntrospection,
    >
{
    /// Composes every production tool family from explicit deployment inputs.
    pub fn try_new_production(
        clock: Clock,
        pool: PgPool,
        credentials: FileCredentialAccess,
        code_host_transport: GitHubCodeHostTransport,
        github_egress_policy: GitHubEgressPolicy,
        workspace_root: &Path,
        web_fetch_egress_policy: WebFetchEgressPolicy,
    ) -> Result<Self, DaemonToolsConstructionError> {
        let web_fetch = WebFetchTool::try_new_production(web_fetch_egress_policy)
            .map_err(|_| DaemonToolsConstructionError::WebFetch)?;
        let status = SessionStatusTool::try_new_postgres(pool.clone())
            .map_err(|_| DaemonToolsConstructionError::SessionStatus)?;
        let code_host = CodeHostTools::try_new(credentials.clone(), code_host_transport)
            .map_err(|_| DaemonToolsConstructionError::CodeHost)?;
        let github = GitHubTools::try_new_production(credentials, github_egress_policy)
            .map_err(|_| DaemonToolsConstructionError::GitHub)?;
        let workspace_read = WorkspaceReadTools::try_new(LocalWorkspaceFileSystem, workspace_root)
            .map_err(|_| DaemonToolsConstructionError::WorkspaceRead)?;
        let workspace_mutation =
            WorkspaceMutationTools::try_new(LocalWorkspaceFileSystem, workspace_root)
                .map_err(|_| DaemonToolsConstructionError::WorkspaceMutation)?;
        let conversations =
            ConversationTools::try_new(PostgresConversationIntrospection::new(pool))
                .map_err(|_| DaemonToolsConstructionError::Conversations)?;
        Self::try_new_with_tools(
            clock,
            ComposedToolFamilies {
                web_fetch,
                status,
                code_host,
                github,
                workspace_read,
                workspace_mutation,
                conversations,
            },
        )
    }
}

impl<Clock, Transport, Writer, Credentials, HostTransport, GitHubTransportType, FileSystem, Port>
    DaemonTools<
        Clock,
        Transport,
        Writer,
        Credentials,
        HostTransport,
        GitHubTransportType,
        FileSystem,
        Port,
    >
where
    FileSystem: WorkspaceFileSystem + WorkspaceMutationFileSystem,
{
    /// Composes every family around injected test or production boundaries.
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        clock: Clock,
        transport: Transport,
        writer: Writer,
        code_host_credentials: Credentials,
        code_host_transport: HostTransport,
        github_credentials: Credentials,
        github_transport: GitHubTransportType,
        github_egress_policy: GitHubEgressPolicy,
        filesystem: FileSystem,
        workspace_root: &Path,
        conversation_port: Port,
        web_fetch_egress_policy: WebFetchEgressPolicy,
    ) -> Result<Self, DaemonToolsConstructionError> {
        let web_fetch = WebFetchTool::try_new(transport, web_fetch_egress_policy)
            .map_err(|_| DaemonToolsConstructionError::WebFetch)?;
        let status = SessionStatusTool::try_new(writer)
            .map_err(|_| DaemonToolsConstructionError::SessionStatus)?;
        let code_host = CodeHostTools::try_new(code_host_credentials, code_host_transport)
            .map_err(|_| DaemonToolsConstructionError::CodeHost)?;
        let github =
            GitHubTools::try_new(github_credentials, github_transport, github_egress_policy)
                .map_err(|_| DaemonToolsConstructionError::GitHub)?;
        let workspace_read = WorkspaceReadTools::try_new(filesystem.clone(), workspace_root)
            .map_err(|_| DaemonToolsConstructionError::WorkspaceRead)?;
        let workspace_mutation = WorkspaceMutationTools::try_new(filesystem, workspace_root)
            .map_err(|_| DaemonToolsConstructionError::WorkspaceMutation)?;
        let conversations = ConversationTools::try_new(conversation_port)
            .map_err(|_| DaemonToolsConstructionError::Conversations)?;
        Self::try_new_with_tools(
            clock,
            ComposedToolFamilies {
                web_fetch,
                status,
                code_host,
                github,
                workspace_read,
                workspace_mutation,
                conversations,
            },
        )
    }

    fn try_new_with_tools(
        clock: Clock,
        families: ComposedToolFamilies<
            Transport,
            Writer,
            Credentials,
            HostTransport,
            GitHubTransportType,
            FileSystem,
            Port,
        >,
    ) -> Result<Self, DaemonToolsConstructionError> {
        let ComposedToolFamilies {
            web_fetch,
            status,
            code_host,
            github,
            workspace_read,
            workspace_mutation,
            conversations,
        } = families;
        let (current_time_catalog, current_time) = CurrentTimeTool::try_new(clock)
            .map_err(|_| DaemonToolsConstructionError::CurrentTime)?
            .into_parts();
        let (echo_catalog, echo) = EchoTool::try_new()
            .map_err(|_| DaemonToolsConstructionError::Echo)?
            .into_parts();
        let (web_fetch_catalog, web_fetch) = web_fetch.into_parts();
        let (status_catalog, session_status) = status.into_parts();
        let (code_host_catalog, code_host) = code_host.into_parts();
        let (github_catalog, github) = github.into_parts();
        let (workspace_read_catalog, workspace_read) = workspace_read.into_parts();
        let (workspace_mutation_catalog, workspace_mutation) = workspace_mutation.into_parts();
        let (conversation_catalog, conversations) = conversations.into_parts();
        let catalog = DaemonToolCatalog::try_new([
            current_time_catalog,
            echo_catalog,
            web_fetch_catalog,
            status_catalog,
            code_host_catalog,
            github_catalog,
            workspace_read_catalog,
            workspace_mutation_catalog,
            conversation_catalog,
        ])
        .map_err(|_| DaemonToolsConstructionError::Duplicate)?;
        Ok(Self {
            catalog,
            executor: DaemonToolExecutor {
                current_time,
                echo,
                web_fetch,
                session_status,
                code_host,
                github,
                workspace_read,
                workspace_mutation: SharedToolExecutor::new(workspace_mutation),
                conversations,
            },
        })
    }

    /// Returns the catalog and executor as separate composition roles.
    #[allow(clippy::type_complexity)]
    pub fn into_parts(
        self,
    ) -> (
        DaemonToolCatalog,
        DaemonToolExecutor<
            Clock,
            Transport,
            Writer,
            Credentials,
            HostTransport,
            GitHubTransportType,
            FileSystem,
            Port,
        >,
    ) {
        (self.catalog, self.executor)
    }
}

/// Why the daemon-local tool set could not be composed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DaemonToolsConstructionError {
    /// The current-time declaration was invalid.
    CurrentTime,
    /// The echo declaration was invalid.
    Echo,
    /// The web-fetch declaration or transport was invalid.
    WebFetch,
    /// The session-status declaration was invalid.
    SessionStatus,
    /// The code-host declarations, credential boundary, or transport were
    /// invalid.
    CodeHost,
    /// The pull-request tool declarations or transport were invalid.
    GitHub,
    /// The workspace read catalog or pinned root was invalid.
    WorkspaceRead,
    /// The workspace mutation catalog or pinned root was invalid.
    WorkspaceMutation,
    /// The conversation declarations or introspection port were invalid.
    Conversations,
    /// Two declarations unexpectedly shared one name.
    Duplicate,
}

impl fmt::Display for DaemonToolsConstructionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::CurrentTime => "current_time tool construction failed",
            Self::Echo => "echo tool construction failed",
            Self::WebFetch => "web_fetch tool construction failed",
            Self::SessionStatus => "session_status_update tool construction failed",
            Self::CodeHost => "code-host tool suite construction failed",
            Self::GitHub => "GitHub pull-request tool suite construction failed",
            Self::WorkspaceRead => "workspace read tool suite construction failed",
            Self::WorkspaceMutation => "workspace mutation tool suite construction failed",
            Self::Conversations => "conversation tool suite construction failed",
            Self::Duplicate => "daemon tool catalog contains a duplicate name",
        })
    }
}

impl Error for DaemonToolsConstructionError {}

#[derive(Clone, Debug)]
struct DaemonToolCatalogEntry {
    definition: ToolDefinition,
    catalog: CompiledToolCatalog,
}

/// Stable merged view of independently compiled daemon tool modules.
#[derive(Clone, Debug)]
pub struct DaemonToolCatalog {
    entries: BTreeMap<ToolName, DaemonToolCatalogEntry>,
}

impl DaemonToolCatalog {
    fn try_new(
        catalogs: impl IntoIterator<Item = CompiledToolCatalog>,
    ) -> Result<Self, DuplicateDaemonTool> {
        let mut entries = BTreeMap::new();
        for catalog in catalogs {
            for definition in catalog.definitions() {
                let name = definition.name().clone();
                if entries
                    .insert(
                        name.clone(),
                        DaemonToolCatalogEntry {
                            definition,
                            catalog: catalog.clone(),
                        },
                    )
                    .is_some()
                {
                    return Err(DuplicateDaemonTool);
                }
            }
        }
        Ok(Self { entries })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DuplicateDaemonTool;

impl ToolCatalog for DaemonToolCatalog {
    fn definitions(&self) -> Box<[ToolDefinition]> {
        self.entries
            .values()
            .map(|entry| entry.definition.clone())
            .collect()
    }

    fn definition(&self, name: &ToolName) -> Option<ToolDefinition> {
        self.entries.get(name).map(|entry| entry.definition.clone())
    }

    fn validate_arguments(
        &self,
        name: &ToolName,
        arguments: &NormalizedToolArguments,
    ) -> Result<(), ToolCatalogValidationFailure> {
        self.entries
            .get(name)
            .ok_or(ToolCatalogValidationFailure::UnknownTool)?
            .catalog
            .validate_arguments(name, arguments)
    }
}

struct SharedToolExecutor<Executor> {
    inner: Arc<Mutex<Executor>>,
}

impl<Executor> SharedToolExecutor<Executor> {
    fn new(executor: Executor) -> Self {
        Self {
            inner: Arc::new(Mutex::new(executor)),
        }
    }
}

impl<Executor> Clone for SharedToolExecutor<Executor> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<Executor> fmt::Debug for SharedToolExecutor<Executor> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SharedToolExecutor")
            .finish_non_exhaustive()
    }
}

impl<Executor> ToolExecutor for SharedToolExecutor<Executor>
where
    Executor: ToolExecutor + Send,
{
    type Error = Executor::Error;

    async fn execute(
        &mut self,
        invocation: ToolExecutionInvocation,
    ) -> Result<CorrelatedToolExecutorEvidence, Self::Error> {
        self.inner.lock().await.execute(invocation).await
    }
}

/// Name-directed daemon executor matching [`DaemonToolCatalog`].
#[derive(Clone, Debug)]
pub struct DaemonToolExecutor<
    Clock,
    Transport,
    Writer,
    Credentials,
    HostTransport,
    GitHubTransportType,
    FileSystem: WorkspaceMutationFileSystem,
    Port,
> {
    current_time: CurrentTimeExecutor<Clock>,
    echo: EchoExecutor,
    web_fetch: WebFetchExecutor<Transport>,
    session_status: SessionStatusExecutor<Writer>,
    code_host: CodeHostExecutor<Credentials, HostTransport>,
    github: GitHubExecutor<Credentials, GitHubTransportType>,
    workspace_read: WorkspaceReadExecutor<FileSystem>,
    workspace_mutation: SharedToolExecutor<WorkspaceMutationExecutor<FileSystem>>,
    conversations: ConversationExecutor<Port>,
}

/// Sanitized aggregate executor failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DaemonToolExecutorError {
    class: OperatorFailureClass,
}

impl DaemonToolExecutorError {
    fn from_error(error: &impl ClassifyOperatorFailure) -> Self {
        Self {
            class: error.operator_failure_class(),
        }
    }

    const fn unknown_tool() -> Self {
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

impl<Clock, Transport, Writer, Credentials, HostTransport, GitHubTransportType, FileSystem, Port>
    ToolExecutor
    for DaemonToolExecutor<
        Clock,
        Transport,
        Writer,
        Credentials,
        HostTransport,
        GitHubTransportType,
        FileSystem,
        Port,
    >
where
    Clock: CurrentTimeClock,
    Transport: WebFetchTransport,
    Writer: SessionStatusWriter,
    Credentials: CredentialAccess,
    HostTransport: CodeHostTransport,
    GitHubTransportType: GitHubTransport,
    FileSystem: WorkspaceFileSystem + WorkspaceMutationFileSystem,
    Port: ConversationIntrospectionPort,
{
    type Error = DaemonToolExecutorError;

    async fn execute(
        &mut self,
        invocation: ToolExecutionInvocation,
    ) -> Result<CorrelatedToolExecutorEvidence, Self::Error> {
        match invocation.request().name().as_str() {
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
                .execute(invocation)
                .await
                .map_err(|error| DaemonToolExecutorError::from_error(&error)),
            name if WORKSPACE_READ_TOOL_NAMES.contains(&name) => self
                .workspace_read
                .execute(invocation)
                .await
                .map_err(|error| DaemonToolExecutorError::from_error(&error)),
            name if WORKSPACE_MUTATION_TOOL_NAMES.contains(&name) => self
                .workspace_mutation
                .execute(invocation)
                .await
                .map_err(|error| DaemonToolExecutorError::from_error(&error)),
            name if CONVERSATION_TOOL_NAMES.contains(&name) => self
                .conversations
                .execute(invocation)
                .await
                .map_err(|error| DaemonToolExecutorError::from_error(&error)),
            _ => Err(DaemonToolExecutorError::unknown_tool()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{fmt, time::SystemTime};

    use signalbox_application::ToolCatalog;
    use signalbox_model_runtime::{
        CredentialAccess, CredentialAccessError, CredentialReference, CredentialValue,
    };

    use super::*;
    use crate::{
        APPLY_PATCH_NAME, CHANGE_REQUEST_CHANGED_FILES_NAME, CHANGE_REQUEST_CHECKS_STATUS_NAME,
        CHANGE_REQUEST_CI_JOB_LOG_NAME, CHANGE_REQUEST_COMMENT_NAME,
        CHANGE_REQUEST_CONVERGENCE_STATE_NAME, CHANGE_REQUEST_FILE_PATCH_NAME,
        CHANGE_REQUEST_RERUN_FAILED_JOBS_NAME, CHANGE_REQUEST_REVIEW_THREADS_NAME,
        CHANGE_REQUEST_STACK_STATE_NAME, CHANGE_REQUEST_SUMMARY_NAME,
        CHANGE_REQUEST_THREAD_INVENTORY_NAME, CHANGE_REQUEST_THREAD_REPLY_NAME,
        CHANGE_REQUEST_THREAD_RESOLVE_NAME, EDIT_FILE_NAME, GLOB_FILES_NAME, LIST_DIRECTORY_NAME,
        PULL_REQUEST_DIFF_NAME, PULL_REQUEST_METADATA_NAME, PULL_REQUEST_PUBLISH_REVIEW_NAME,
        PULL_REQUEST_REVIEW_THREADS_NAME, READ_FILE_NAME, REPOSITORY_LIST_DIRECTORY_NAME,
        REPOSITORY_READ_FILE_NAME, REVIEW_GATE_CHECK_NAME, SEARCH_FILES_NAME, SessionStatusWrite,
        SessionStatusWriteOutcome, WRITE_FILE_NAME, WebFetchRequest, WebFetchResponse,
        WebFetchTransportFailure,
    };

    #[derive(Clone, Copy, Debug)]
    struct OfflineTransport;

    impl WebFetchTransport for OfflineTransport {
        async fn fetch(
            &mut self,
            _request: WebFetchRequest,
        ) -> Result<WebFetchResponse, WebFetchTransportFailure> {
            Err(WebFetchTransportFailure::RequestFailed)
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct OfflineWriterError;

    impl fmt::Display for OfflineWriterError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("offline writer is not invoked")
        }
    }

    impl Error for OfflineWriterError {}

    impl ClassifyOperatorFailure for OfflineWriterError {
        fn operator_failure_class(&self) -> OperatorFailureClass {
            OperatorFailureClass::CallerOrHubBug
        }
    }

    #[derive(Clone, Copy, Debug)]
    struct OfflineWriter;

    impl SessionStatusWriter for OfflineWriter {
        type Error = OfflineWriterError;

        async fn write(
            &mut self,
            _update: SessionStatusWrite,
        ) -> Result<SessionStatusWriteOutcome, Self::Error> {
            Err(OfflineWriterError)
        }
    }

    #[derive(Clone, Copy, Debug)]
    struct OfflineCredentials;

    impl CredentialAccess for OfflineCredentials {
        async fn resolve(
            &self,
            _reference: &CredentialReference,
        ) -> Result<CredentialValue, CredentialAccessError> {
            Ok(CredentialValue::new(b"offline-token".to_vec()))
        }
    }

    #[derive(Clone, Copy, Debug)]
    struct OfflineCodeHostTransport;

    impl CodeHostTransport for OfflineCodeHostTransport {
        async fn execute(
            &mut self,
            _operation: crate::CodeHostOperation,
            _credential: &CredentialValue,
        ) -> Result<crate::CodeHostResult, crate::CodeHostTransportFailure> {
            Err(crate::CodeHostTransportFailure::Rejected)
        }
    }

    #[derive(Clone, Copy, Debug)]
    struct OfflineGitHubTransport;

    impl GitHubTransport for OfflineGitHubTransport {
        async fn execute(
            &mut self,
            _operation: crate::GitHubOperation,
            _credential: &CredentialValue,
            _egress_policy: &GitHubEgressPolicy,
        ) -> Result<crate::GitHubResult, crate::GitHubTransportFailure> {
            Err(crate::GitHubTransportFailure::PreDispatchInfrastructure)
        }
    }

    #[derive(Clone, Copy, Debug)]
    struct OfflineConversationPort;

    impl ConversationIntrospectionPort for OfflineConversationPort {
        type Error = OfflineWriterError;

        async fn list_conversations(
            &mut self,
            _request: signalbox_tools_conversations::ConversationListRequest,
        ) -> Result<signalbox_tools_conversations::ConversationListPage, Self::Error> {
            Ok(signalbox_tools_conversations::ConversationListPage::new(
                Vec::new(),
                false,
            ))
        }

        async fn read_conversation(
            &mut self,
            _request: signalbox_tools_conversations::ConversationTranscriptRequest,
        ) -> Result<Option<signalbox_tools_conversations::TranscriptPage>, Self::Error> {
            Ok(None)
        }

        async fn read_imported_conversation(
            &mut self,
            _request: signalbox_tools_conversations::ImportedTranscriptRequest,
        ) -> Result<Option<signalbox_tools_conversations::TranscriptPage>, Self::Error> {
            Ok(None)
        }
    }

    fn definition_names(definitions: &[ToolDefinition]) -> Vec<&str> {
        definitions
            .iter()
            .map(|definition| definition.name().as_str())
            .collect()
    }

    /// The merged process-lifetime catalog exposes every daemon declaration in
    /// deterministic name order.
    #[test]
    fn daemon_catalog_contains_every_injected_tool_family() {
        let workspace = tempfile::tempdir().expect("workspace root exists");
        let (catalog, _executor) = DaemonTools::try_new(
            || SystemTime::UNIX_EPOCH,
            OfflineTransport,
            OfflineWriter,
            OfflineCredentials,
            OfflineCodeHostTransport,
            OfflineCredentials,
            OfflineGitHubTransport,
            GitHubEgressPolicy::github_api_only(),
            LocalWorkspaceFileSystem,
            workspace.path(),
            OfflineConversationPort,
            WebFetchEgressPolicy::deny_all(),
        )
        .expect("static daemon tools compile")
        .into_parts();

        let definitions = catalog.definitions();
        let names = definition_names(&definitions);

        assert_eq!(
            names,
            [
                APPLY_PATCH_NAME,
                CHANGE_REQUEST_CHANGED_FILES_NAME,
                CHANGE_REQUEST_CHECKS_STATUS_NAME,
                CHANGE_REQUEST_CI_JOB_LOG_NAME,
                CHANGE_REQUEST_COMMENT_NAME,
                CHANGE_REQUEST_CONVERGENCE_STATE_NAME,
                CHANGE_REQUEST_FILE_PATCH_NAME,
                CHANGE_REQUEST_RERUN_FAILED_JOBS_NAME,
                CHANGE_REQUEST_REVIEW_THREADS_NAME,
                CHANGE_REQUEST_STACK_STATE_NAME,
                CHANGE_REQUEST_SUMMARY_NAME,
                CHANGE_REQUEST_THREAD_INVENTORY_NAME,
                CHANGE_REQUEST_THREAD_REPLY_NAME,
                CHANGE_REQUEST_THREAD_RESOLVE_NAME,
                CURRENT_TIME_NAME,
                ECHO_NAME,
                EDIT_FILE_NAME,
                PULL_REQUEST_DIFF_NAME,
                PULL_REQUEST_METADATA_NAME,
                PULL_REQUEST_PUBLISH_REVIEW_NAME,
                PULL_REQUEST_REVIEW_THREADS_NAME,
                GLOB_FILES_NAME,
                signalbox_tools_conversations::LIST_CONVERSATIONS_NAME,
                LIST_DIRECTORY_NAME,
                signalbox_tools_conversations::READ_CONVERSATION_NAME,
                READ_FILE_NAME,
                signalbox_tools_conversations::READ_IMPORTED_CONVERSATION_NAME,
                signalbox_tools_conversations::READ_OWN_CONVERSATION_NAME,
                REPOSITORY_LIST_DIRECTORY_NAME,
                REPOSITORY_READ_FILE_NAME,
                REVIEW_GATE_CHECK_NAME,
                SEARCH_FILES_NAME,
                SESSION_STATUS_UPDATE_NAME,
                WEB_FETCH_NAME,
                WRITE_FILE_NAME,
            ]
        );
    }
}
