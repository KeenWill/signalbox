//! Process-lifetime compiled daemon tool catalog and executor dispatch.

use std::{collections::BTreeMap, error::Error, fmt, path::Path, sync::Arc};

use signalbox_application::{
    ClassifyOperatorFailure, CompiledToolCatalog, CorrelatedDurableChildWait,
    CorrelatedToolExecutorEvidence, OperatorFailureClass, ToolCatalog,
    ToolCatalogValidationFailure, ToolDefinition, ToolExecutionInvocation, ToolExecutor,
    ToolExecutorDisposition,
};
use signalbox_domain::{NormalizedToolArguments, ToolApprovalPosture, ToolName};
use signalbox_model_runtime::CredentialAccess;
use signalbox_persistence::plan::SessionPlanRepository;
use signalbox_tools_basic::{
    CURRENT_TIME_NAME, CurrentTimeClock, CurrentTimeExecutor, CurrentTimeTool, ECHO_NAME,
    EchoExecutor, EchoTool, PostgresSessionStatusWriter, SESSION_STATUS_UPDATE_NAME,
    SessionStatusExecutor, SessionStatusTool, SessionStatusWriter,
};
use signalbox_tools_code_host::{
    CODE_HOST_TOOL_NAMES, CodeHostExecutor, CodeHostTools, CodeHostTransport,
    GitHubCodeHostTransport,
};
use signalbox_tools_conversations::{
    CONVERSATION_TOOL_NAMES, ConversationExecutor, ConversationIntrospectionPort, ConversationTools,
};
use signalbox_tools_exec::{
    CARGO_DIAGNOSTICS_NAME, CargoDiagnosticsExecutor, CargoDiagnosticsTool, ExecExecutor,
    ProcessRunner, SANDBOXED_EXEC_NAME, SandboxedCommandRunner, SandboxedExecTool,
    TokioProcessRunner, UNSANDBOXED_EXEC_NAME, UnsandboxedCommandRunner, UnsandboxedExecTool,
};
use signalbox_tools_git::{GitIdentity, LOCAL_GIT_TOOL_NAMES, LocalGitExecutor, LocalGitTools};
use signalbox_tools_github::{
    GITHUB_TOOL_NAMES, GitHubApiTransport, GitHubEgressPolicy, GitHubExecutor, GitHubTools,
    GitHubTransport,
};
use signalbox_tools_plan::{PLAN_TOOL_NAMES, PlanExecutor, PlanTools, SessionPlanPort};
use signalbox_tools_sessions::{
    SESSION_DELEGATION_TOOL_NAMES, SessionDelegationExecutionDisposition,
    SessionDelegationExecutor, SessionDelegationTools,
};
use signalbox_tools_web::{
    ReqwestWebFetchTransport, ReqwestWebSearchTransport, WEB_FETCH_NAME, WEB_SEARCH_NAME,
    WebFetchEgressPolicy, WebFetchExecutor, WebFetchTool, WebFetchTransport,
    WebSearchConfiguration, WebSearchExecutor, WebSearchProvider, WebSearchTool,
    WebSearchTransport,
};
use signalbox_tools_workspace::{
    LocalWorkspaceFileSystem, WORKSPACE_MUTATION_TOOL_NAMES, WORKSPACE_READ_TOOL_NAMES,
    WorkspaceDirectoryRead, WorkspaceEntryKind, WorkspaceFileBytes, WorkspaceFileMutation,
    WorkspaceFileSystem, WorkspaceMutationCommitError, WorkspaceMutationExecutor,
    WorkspaceMutationFileSystem, WorkspaceMutationPath, WorkspaceMutationSnapshot,
    WorkspaceMutationSnapshotError, WorkspaceMutationTools, WorkspaceReadExecutor,
    WorkspaceReadTools, WorkspaceResolveError, WorkspaceRoot, WorkspaceRootError,
};
use sqlx::PgPool;
use tokio::sync::Mutex;

use crate::{
    FileCredentialAccess, PostgresConversationIntrospection,
    goal_mode::{GOAL_DECLARE_NAME, GoalDeclarationExecutor, GoalDeclarationTool},
    session_delegation::DaemonSessionDelegationPort,
};

/// Daemon-local filesystem adapter that shares one pinned root across both
/// workspace suites.
#[derive(Clone, Debug)]
pub struct PinnedWorkspaceFileSystem {
    root: WorkspaceRoot,
    local: LocalWorkspaceFileSystem,
}

impl PinnedWorkspaceFileSystem {
    /// Opens the configured root exactly once for process-lifetime sharing.
    pub fn try_new(root: &Path) -> Result<Self, WorkspaceRootError> {
        let local = LocalWorkspaceFileSystem;
        let root = WorkspaceRoot::try_new(&local, root)?;
        Ok(Self { root, local })
    }
}

impl WorkspaceFileSystem for PinnedWorkspaceFileSystem {
    fn open_root(&self, _root: &Path) -> Result<WorkspaceRoot, WorkspaceRootError> {
        Ok(self.root.clone())
    }

    fn entry_kind(
        &self,
        root: &WorkspaceRoot,
        path: &Path,
    ) -> Result<WorkspaceEntryKind, WorkspaceResolveError> {
        self.local.entry_kind(root, path)
    }

    fn read_directory(
        &self,
        root: &WorkspaceRoot,
        path: &Path,
        max_entries: usize,
        max_inspections: usize,
        max_path_bytes: usize,
    ) -> Result<WorkspaceDirectoryRead, WorkspaceResolveError> {
        self.local
            .read_directory(root, path, max_entries, max_inspections, max_path_bytes)
    }

    fn read_file_prefix(
        &self,
        root: &WorkspaceRoot,
        path: &Path,
        max_bytes: usize,
    ) -> Result<WorkspaceFileBytes, WorkspaceResolveError> {
        self.local.read_file_prefix(root, path, max_bytes)
    }
}

impl WorkspaceMutationFileSystem for PinnedWorkspaceFileSystem {
    type Root = WorkspaceRoot;

    fn open_root(&self, _root: &Path) -> Result<Self::Root, WorkspaceMutationSnapshotError> {
        Ok(self.root.clone())
    }

    fn snapshot(
        &self,
        root: &Self::Root,
        paths: &[WorkspaceMutationPath],
        max_file_bytes: usize,
    ) -> Result<WorkspaceMutationSnapshot, WorkspaceMutationSnapshotError> {
        self.local.snapshot(root, paths, max_file_bytes)
    }

    fn commit_atomically(
        &self,
        root: &Self::Root,
        expected: &WorkspaceMutationSnapshot,
        mutations: &[WorkspaceFileMutation],
    ) -> Result<(), WorkspaceMutationCommitError> {
        self.local.commit_atomically(root, expected, mutations)
    }
}

struct ComposedToolFamilies<
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
    web_fetch: WebFetchTool<Transport>,
    web_search: WebSearchTool<Credentials, SearchTransport>,
    status: SessionStatusTool<Writer>,
    code_host: CodeHostTools<Credentials, HostTransport>,
    github: Option<GitHubTools<Credentials, GitHubTransportType>>,
    workspace_read: Option<WorkspaceReadTools<FileSystem>>,
    workspace_mutation: Option<WorkspaceMutationTools<FileSystem>>,
    local_git: Option<LocalGitTools<FileSystem>>,
    sandboxed_exec: Option<SandboxedExecTool<ExecRunner>>,
    unsandboxed_exec: Option<UnsandboxedExecTool<ExecRunner>>,
    cargo_diagnostics: Option<CargoDiagnosticsTool<ExecRunner>>,
    conversations: Option<ConversationTools<ConversationPort>>,
    plan: PlanTools<PlanPort>,
    delegation: SessionDelegationTools<DaemonSessionDelegationPort>,
    goal: Option<GoalDeclarationTool>,
}

/// Credential channels required by the daemon's base tool composition.
pub struct BaseDaemonCredentialInputs<Credentials> {
    /// Credential access for authenticated web search.
    pub web_search: Credentials,
    /// Credential access shared by the base code-host tools.
    pub code_host: Credentials,
}

/// Credential channels required when every mapped daemon family is composed.
pub struct MappedDaemonCredentialInputs<Credentials> {
    /// Credential access for authenticated web search.
    pub web_search: Credentials,
    /// Credential access for code-host tools.
    pub code_host: Credentials,
    /// Credential access for the mapped GitHub family.
    pub github: Credentials,
}

/// The complete daemon-local declarations and their matching dispatch executor.
pub struct DaemonTools<
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
    ExecRunner: ProcessRunner = TokioProcessRunner,
> {
    catalog: DaemonToolCatalog,
    executor: DaemonToolExecutor<
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
    >,
}

impl<Clock>
    DaemonTools<
        Clock,
        ReqwestWebFetchTransport,
        ReqwestWebSearchTransport,
        PostgresSessionStatusWriter,
        FileCredentialAccess,
        GitHubCodeHostTransport,
        GitHubApiTransport,
        PinnedWorkspaceFileSystem,
        PostgresConversationIntrospection,
        SessionPlanRepository,
    >
{
    /// Composes every production tool family from explicit deployment inputs.
    #[allow(clippy::too_many_arguments)]
    pub fn try_new_production(
        clock: Clock,
        pool: PgPool,
        credentials: MappedDaemonCredentialInputs<FileCredentialAccess>,
        code_host_transport: GitHubCodeHostTransport,
        github_egress_policy: GitHubEgressPolicy,
        workspace_root: &Path,
        git_identity: GitIdentity,
        exec_supervisor_executable: &Path,
        web_fetch_egress_policy: WebFetchEgressPolicy,
    ) -> Result<Self, DaemonToolsConstructionError> {
        let MappedDaemonCredentialInputs {
            web_search,
            code_host,
            github,
        } = credentials;
        let web_fetch = WebFetchTool::try_new_production(web_fetch_egress_policy)
            .map_err(|_| DaemonToolsConstructionError::WebFetch)?;
        let web_search = WebSearchTool::try_new_production(
            web_search,
            WebSearchConfiguration::new(WebSearchProvider::Brave),
        )
        .map_err(|_| DaemonToolsConstructionError::WebSearch)?;
        let status = SessionStatusTool::try_new_postgres(pool.clone())
            .map_err(|_| DaemonToolsConstructionError::SessionStatus)?;
        let code_host = CodeHostTools::try_new(code_host, code_host_transport)
            .map_err(|_| DaemonToolsConstructionError::CodeHost)?;
        let github = GitHubTools::try_new_production(github, github_egress_policy)
            .map_err(|_| DaemonToolsConstructionError::GitHub)?;
        let workspace = PinnedWorkspaceFileSystem::try_new(workspace_root)
            .map_err(|_| DaemonToolsConstructionError::WorkspaceRead)?;
        let workspace_read = WorkspaceReadTools::try_new(workspace.clone(), workspace_root)
            .map_err(|_| DaemonToolsConstructionError::WorkspaceRead)?;
        let workspace_mutation = WorkspaceMutationTools::try_new(workspace.clone(), workspace_root)
            .map_err(|_| DaemonToolsConstructionError::WorkspaceMutation)?;
        let local_git = LocalGitTools::try_new(workspace, workspace_root, git_identity)
            .map_err(|_| DaemonToolsConstructionError::LocalGit)?;
        let exec_runner = TokioProcessRunner::try_new(exec_supervisor_executable)
            .map_err(|_| DaemonToolsConstructionError::Exec)?;
        let sandboxed_exec = SandboxedExecTool::try_new(exec_runner.clone(), workspace_root)
            .map_err(|_| DaemonToolsConstructionError::Exec)?;
        let unsandboxed_exec = UnsandboxedExecTool::try_new(exec_runner.clone(), workspace_root)
            .map_err(|_| DaemonToolsConstructionError::Exec)?;
        let cargo_diagnostics = CargoDiagnosticsTool::try_new(exec_runner, workspace_root)
            .map_err(|_| DaemonToolsConstructionError::Exec)?;
        let conversations =
            ConversationTools::try_new(PostgresConversationIntrospection::new(pool.clone()))
                .map_err(|_| DaemonToolsConstructionError::Conversations)?;
        let goal = GoalDeclarationTool::try_new(pool.clone())
            .map_err(|_| DaemonToolsConstructionError::GoalDeclaration)?;
        let delegation =
            SessionDelegationTools::try_new(DaemonSessionDelegationPort::postgres(pool.clone()))
                .map_err(|_| DaemonToolsConstructionError::SessionDelegation)?;
        let plan = PlanTools::try_new(SessionPlanRepository::new(pool))
            .map_err(|_| DaemonToolsConstructionError::Plan)?;
        Self::try_new_with_tools(
            clock,
            ComposedToolFamilies {
                web_fetch,
                web_search,
                status,
                code_host,
                github: Some(github),
                workspace_read: Some(workspace_read),
                workspace_mutation: Some(workspace_mutation),
                local_git: Some(local_git),
                sandboxed_exec: Some(sandboxed_exec),
                unsandboxed_exec: Some(unsandboxed_exec),
                cargo_diagnostics: Some(cargo_diagnostics),
                conversations: Some(conversations),
                plan,
                delegation,
                goal: Some(goal),
            },
        )
    }

    /// Composes the base production catalog without constructing any dependency
    /// owned by an unconfigured tool family.
    pub fn try_new_without_tool_mappings(
        clock: Clock,
        pool: PgPool,
        credentials: BaseDaemonCredentialInputs<FileCredentialAccess>,
        code_host_transport: GitHubCodeHostTransport,
        web_fetch_egress_policy: WebFetchEgressPolicy,
    ) -> Result<Self, DaemonToolsConstructionError> {
        let BaseDaemonCredentialInputs {
            web_search,
            code_host,
        } = credentials;
        let web_fetch = WebFetchTool::try_new_production(web_fetch_egress_policy)
            .map_err(|_| DaemonToolsConstructionError::WebFetch)?;
        let web_search = WebSearchTool::try_new_production(
            web_search,
            WebSearchConfiguration::new(WebSearchProvider::Brave),
        )
        .map_err(|_| DaemonToolsConstructionError::WebSearch)?;
        let status = SessionStatusTool::try_new_postgres(pool.clone())
            .map_err(|_| DaemonToolsConstructionError::SessionStatus)?;
        let goal = GoalDeclarationTool::try_new(pool.clone())
            .map_err(|_| DaemonToolsConstructionError::GoalDeclaration)?;
        let code_host = CodeHostTools::try_new(code_host, code_host_transport)
            .map_err(|_| DaemonToolsConstructionError::CodeHost)?;
        let delegation =
            SessionDelegationTools::try_new(DaemonSessionDelegationPort::postgres(pool.clone()))
                .map_err(|_| DaemonToolsConstructionError::SessionDelegation)?;
        let plan = PlanTools::try_new(SessionPlanRepository::new(pool))
            .map_err(|_| DaemonToolsConstructionError::Plan)?;
        Self::try_new_with_tools(
            clock,
            ComposedToolFamilies {
                web_fetch,
                web_search,
                status,
                code_host,
                github: None,
                workspace_read: None,
                workspace_mutation: None,
                local_git: None,
                sandboxed_exec: None,
                unsandboxed_exec: None,
                cargo_diagnostics: None,
                conversations: None,
                plan,
                delegation,
                goal: Some(goal),
            },
        )
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
>
    DaemonTools<
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
    FileSystem: WorkspaceFileSystem + WorkspaceMutationFileSystem,
    ExecRunner: ProcessRunner,
{
    /// Composes every family around injected test or production boundaries.
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        clock: Clock,
        transport: Transport,
        credentials: MappedDaemonCredentialInputs<Credentials>,
        web_search_transport: SearchTransport,
        writer: Writer,
        code_host_transport: HostTransport,
        github_transport: GitHubTransportType,
        github_egress_policy: GitHubEgressPolicy,
        filesystem: FileSystem,
        workspace_root: &Path,
        git_identity: GitIdentity,
        exec_runner: ExecRunner,
        conversation_port: ConversationPort,
        plan_port: PlanPort,
        web_fetch_egress_policy: WebFetchEgressPolicy,
    ) -> Result<Self, DaemonToolsConstructionError> {
        let MappedDaemonCredentialInputs {
            web_search,
            code_host,
            github,
        } = credentials;
        let web_fetch = WebFetchTool::try_new(transport, web_fetch_egress_policy)
            .map_err(|_| DaemonToolsConstructionError::WebFetch)?;
        let web_search = WebSearchTool::try_new(
            web_search,
            web_search_transport,
            WebSearchConfiguration::new(WebSearchProvider::Brave),
        )
        .map_err(|_| DaemonToolsConstructionError::WebSearch)?;
        let status = SessionStatusTool::try_new(writer)
            .map_err(|_| DaemonToolsConstructionError::SessionStatus)?;
        let code_host = CodeHostTools::try_new(code_host, code_host_transport)
            .map_err(|_| DaemonToolsConstructionError::CodeHost)?;
        let github = GitHubTools::try_new(github, github_transport, github_egress_policy)
            .map_err(|_| DaemonToolsConstructionError::GitHub)?;
        let workspace_read = WorkspaceReadTools::try_new(filesystem.clone(), workspace_root)
            .map_err(|_| DaemonToolsConstructionError::WorkspaceRead)?;
        let workspace_mutation =
            WorkspaceMutationTools::try_new(filesystem.clone(), workspace_root)
                .map_err(|_| DaemonToolsConstructionError::WorkspaceMutation)?;
        let local_git = LocalGitTools::try_new(filesystem, workspace_root, git_identity)
            .map_err(|_| DaemonToolsConstructionError::LocalGit)?;
        let sandboxed_exec = SandboxedExecTool::try_new(exec_runner.clone(), workspace_root)
            .map_err(|_| DaemonToolsConstructionError::Exec)?;
        let unsandboxed_exec = UnsandboxedExecTool::try_new(exec_runner.clone(), workspace_root)
            .map_err(|_| DaemonToolsConstructionError::Exec)?;
        let cargo_diagnostics = CargoDiagnosticsTool::try_new(exec_runner, workspace_root)
            .map_err(|_| DaemonToolsConstructionError::Exec)?;
        let conversations = ConversationTools::try_new(conversation_port)
            .map_err(|_| DaemonToolsConstructionError::Conversations)?;
        let plan = PlanTools::try_new(plan_port).map_err(|_| DaemonToolsConstructionError::Plan)?;
        let delegation =
            SessionDelegationTools::try_new(DaemonSessionDelegationPort::unavailable())
                .map_err(|_| DaemonToolsConstructionError::SessionDelegation)?;
        Self::try_new_with_tools(
            clock,
            ComposedToolFamilies {
                web_fetch,
                web_search,
                status,
                code_host,
                github: Some(github),
                workspace_read: Some(workspace_read),
                workspace_mutation: Some(workspace_mutation),
                local_git: Some(local_git),
                sandboxed_exec: Some(sandboxed_exec),
                unsandboxed_exec: Some(unsandboxed_exec),
                cargo_diagnostics: Some(cargo_diagnostics),
                conversations: Some(conversations),
                plan,
                delegation,
                goal: None,
            },
        )
    }

    fn try_new_with_tools(
        clock: Clock,
        families: ComposedToolFamilies<
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
        >,
    ) -> Result<Self, DaemonToolsConstructionError> {
        let ComposedToolFamilies {
            web_fetch,
            web_search,
            status,
            code_host,
            github,
            workspace_read,
            workspace_mutation,
            local_git,
            sandboxed_exec,
            unsandboxed_exec,
            cargo_diagnostics,
            conversations,
            plan,
            delegation,
            goal,
        } = families;
        let (current_time_catalog, current_time) = CurrentTimeTool::try_new(clock)
            .map_err(|_| DaemonToolsConstructionError::CurrentTime)?
            .into_parts();
        let (echo_catalog, echo) = EchoTool::try_new()
            .map_err(|_| DaemonToolsConstructionError::Echo)?
            .into_parts();
        let (web_fetch_catalog, web_fetch) = web_fetch.into_parts();
        let (web_search_catalog, web_search) = web_search.into_parts();
        let (status_catalog, session_status) = status.into_parts();
        let (code_host_catalog, code_host) = code_host.into_parts();
        let github = github.map(GitHubTools::into_parts);
        let workspace_read = workspace_read.map(WorkspaceReadTools::into_parts);
        let workspace_mutation = workspace_mutation.map(WorkspaceMutationTools::into_parts);
        let local_git = local_git.map(LocalGitTools::into_parts);
        let sandboxed_exec = sandboxed_exec.map(SandboxedExecTool::into_parts);
        let unsandboxed_exec = unsandboxed_exec.map(UnsandboxedExecTool::into_parts);
        let cargo_diagnostics = cargo_diagnostics.map(CargoDiagnosticsTool::into_parts);
        let conversations = conversations.map(ConversationTools::into_parts);
        let (plan_catalog, plan) = plan.into_parts();
        let (delegation_catalog, delegation) = delegation.into_parts();
        let goal = goal.map(GoalDeclarationTool::into_parts);
        let mut catalogs = vec![
            current_time_catalog,
            echo_catalog,
            web_fetch_catalog,
            web_search_catalog,
            status_catalog,
            code_host_catalog,
            plan_catalog,
            delegation_catalog,
        ];
        catalogs.extend(github.as_ref().map(|(catalog, _)| catalog.clone()));
        catalogs.extend(workspace_read.as_ref().map(|(catalog, _)| catalog.clone()));
        catalogs.extend(
            workspace_mutation
                .as_ref()
                .map(|(catalog, _)| catalog.clone()),
        );
        catalogs.extend(local_git.as_ref().map(|(catalog, _)| catalog.clone()));
        catalogs.extend(sandboxed_exec.as_ref().map(|(catalog, _)| catalog.clone()));
        catalogs.extend(
            unsandboxed_exec
                .as_ref()
                .map(|(catalog, _)| catalog.clone()),
        );
        catalogs.extend(
            cargo_diagnostics
                .as_ref()
                .map(|(catalog, _)| catalog.clone()),
        );
        catalogs.extend(conversations.as_ref().map(|(catalog, _)| catalog.clone()));
        catalogs.extend(goal.as_ref().map(|(catalog, _)| catalog.clone()));
        let catalog = DaemonToolCatalog::try_new(catalogs)
            .map_err(|_| DaemonToolsConstructionError::Duplicate)?;
        Ok(Self {
            catalog,
            executor: DaemonToolExecutor {
                current_time,
                echo,
                web_fetch,
                web_search,
                session_status,
                code_host,
                github: github.map(|(_, executor)| executor),
                workspace_read: workspace_read.map(|(_, executor)| executor),
                workspace_mutation: workspace_mutation
                    .map(|(_, executor)| SharedToolExecutor::new(executor)),
                local_git: local_git.map(|(_, executor)| SharedToolExecutor::new(executor)),
                sandboxed_exec: sandboxed_exec.map(|(_, executor)| executor),
                unsandboxed_exec: unsandboxed_exec.map(|(_, executor)| executor),
                cargo_diagnostics: cargo_diagnostics.map(|(_, executor)| executor),
                conversations: conversations.map(|(_, executor)| executor),
                plan,
                delegation,
                goal: goal.map(|(_, executor)| executor),
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
            SearchTransport,
            Writer,
            Credentials,
            HostTransport,
            GitHubTransportType,
            FileSystem,
            ConversationPort,
            PlanPort,
            ExecRunner,
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
    /// The web-search declaration or transport was invalid.
    WebSearch,
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
    /// The local Git catalog, repository root, or identity was invalid.
    LocalGit,
    /// The execution catalogs, workspace root, or supervisor program were
    /// invalid.
    Exec,
    /// The conversation declarations or introspection port were invalid.
    Conversations,
    /// The plan declarations or session plan port were invalid.
    Plan,
    /// The session-delegation declarations were invalid.
    SessionDelegation,
    /// The goal declaration or its static validation details were invalid.
    GoalDeclaration,
    /// Two declarations unexpectedly shared one name.
    Duplicate,
}

impl fmt::Display for DaemonToolsConstructionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::CurrentTime => "current_time tool construction failed",
            Self::Echo => "echo tool construction failed",
            Self::WebFetch => "web_fetch tool construction failed",
            Self::WebSearch => "web_search tool construction failed",
            Self::SessionStatus => "session_status_update tool construction failed",
            Self::CodeHost => "code-host tool suite construction failed",
            Self::GitHub => "GitHub pull-request tool suite construction failed",
            Self::WorkspaceRead => "workspace read tool suite construction failed",
            Self::WorkspaceMutation => "workspace mutation tool suite construction failed",
            Self::LocalGit => "local Git tool suite construction failed",
            Self::Exec => "exec tool suite construction failed",
            Self::Conversations => "conversation tool suite construction failed",
            Self::Plan => "plan tool suite construction failed",
            Self::SessionDelegation => "session-delegation tool suite construction failed",
            Self::GoalDeclaration => "goal_declare tool construction failed",
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

/// Statically selected daemon tool families available before runtime assembly.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DaemonToolComposition {
    /// Process-local and always-compiled tool families only.
    Base,
    /// Base tools plus families enabled by complete deployment mappings.
    WithMappedFamilies,
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

    /// Validates deployment postures against the statically selected
    /// composition before database-backed tool dependencies are constructed.
    pub fn validate_approval_postures_for_composition(
        postures: impl IntoIterator<Item = (ToolName, ToolApprovalPosture)>,
        composition: DaemonToolComposition,
    ) -> Result<(), ConfiguredApprovalPostureError> {
        for (name, _posture) in postures {
            if !configured_composition_contains(&name, composition) {
                return Err(ConfiguredApprovalPostureError::UnknownTool { name });
            }
        }
        Ok(())
    }

    /// Applies explicit deployment postures that the current runtime can enforce.
    pub fn with_approval_postures(
        mut self,
        postures: impl IntoIterator<Item = (ToolName, ToolApprovalPosture)>,
    ) -> Result<Self, ConfiguredApprovalPostureError> {
        for (name, posture) in postures {
            let Some(entry) = self.entries.get_mut(&name) else {
                return Err(ConfiguredApprovalPostureError::UnknownTool { name });
            };
            entry.definition = entry.definition.clone().with_approval_posture(posture);
        }
        Ok(self)
    }
}

fn configured_composition_contains(name: &ToolName, composition: DaemonToolComposition) -> bool {
    let name = name.as_str();
    let mapped_family_contains = match composition {
        DaemonToolComposition::Base => false,
        DaemonToolComposition::WithMappedFamilies => {
            GITHUB_TOOL_NAMES.contains(&name)
                || WORKSPACE_READ_TOOL_NAMES.contains(&name)
                || WORKSPACE_MUTATION_TOOL_NAMES.contains(&name)
                || LOCAL_GIT_TOOL_NAMES.contains(&name)
                || matches!(
                    name,
                    SANDBOXED_EXEC_NAME | UNSANDBOXED_EXEC_NAME | CARGO_DIAGNOSTICS_NAME
                )
                || CONVERSATION_TOOL_NAMES.contains(&name)
        }
    };
    name == CURRENT_TIME_NAME
        || name == ECHO_NAME
        || name == WEB_FETCH_NAME
        || name == WEB_SEARCH_NAME
        || name == SESSION_STATUS_UPDATE_NAME
        || name == GOAL_DECLARE_NAME
        || CODE_HOST_TOOL_NAMES.contains(&name)
        || PLAN_TOOL_NAMES.contains(&name)
        || SESSION_DELEGATION_TOOL_NAMES.contains(&name)
        || mapped_family_contains
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DuplicateDaemonTool;

/// A configured approval posture cannot be enforced by this daemon runtime.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConfiguredApprovalPostureError {
    /// The configured name is absent from the composed catalog.
    UnknownTool { name: ToolName },
}

impl ConfiguredApprovalPostureError {
    /// Borrows the configured tool name without exposing it to startup telemetry.
    pub const fn name(&self) -> &ToolName {
        match self {
            Self::UnknownTool { name } => name,
        }
    }
}

impl fmt::Display for ConfiguredApprovalPostureError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::UnknownTool { .. } => "configured approval posture names an unknown tool",
        })
    }
}

impl Error for ConfiguredApprovalPostureError {}

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
    current_time: CurrentTimeExecutor<Clock>,
    echo: EchoExecutor,
    web_fetch: WebFetchExecutor<Transport>,
    web_search: WebSearchExecutor<Credentials, SearchTransport>,
    session_status: SessionStatusExecutor<Writer>,
    code_host: CodeHostExecutor<Credentials, HostTransport>,
    github: Option<GitHubExecutor<Credentials, GitHubTransportType>>,
    workspace_read: Option<WorkspaceReadExecutor<FileSystem>>,
    workspace_mutation: Option<SharedToolExecutor<WorkspaceMutationExecutor<FileSystem>>>,
    local_git: Option<SharedToolExecutor<LocalGitExecutor<FileSystem>>>,
    sandboxed_exec: Option<ExecExecutor<SandboxedCommandRunner<ExecRunner>>>,
    unsandboxed_exec: Option<ExecExecutor<UnsandboxedCommandRunner<ExecRunner>>>,
    cargo_diagnostics: Option<CargoDiagnosticsExecutor<ExecRunner>>,
    conversations: Option<ConversationExecutor<ConversationPort>>,
    plan: PlanExecutor<PlanPort>,
    delegation: SessionDelegationExecutor<DaemonSessionDelegationPort>,
    goal: Option<GoalDeclarationExecutor>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DaemonExecRoute {
    Sandboxed,
    Unsandboxed,
    CargoDiagnostics,
}

fn daemon_exec_route(name: &str) -> Option<DaemonExecRoute> {
    match name {
        SANDBOXED_EXEC_NAME => Some(DaemonExecRoute::Sandboxed),
        UNSANDBOXED_EXEC_NAME => Some(DaemonExecRoute::Unsandboxed),
        CARGO_DIAGNOSTICS_NAME => Some(DaemonExecRoute::CargoDiagnostics),
        _ => None,
    }
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
    FileSystem: WorkspaceFileSystem + WorkspaceMutationFileSystem,
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
        if let Some(route) = daemon_exec_route(name) {
            return match route {
                DaemonExecRoute::Sandboxed => self
                    .sandboxed_exec
                    .as_mut()
                    .ok_or_else(DaemonToolExecutorError::unknown_tool)?
                    .execute(invocation)
                    .await
                    .map_err(|error| DaemonToolExecutorError::from_error(&error)),
                DaemonExecRoute::Unsandboxed => self
                    .unsandboxed_exec
                    .as_mut()
                    .ok_or_else(DaemonToolExecutorError::unknown_tool)?
                    .execute(invocation)
                    .await
                    .map_err(|error| DaemonToolExecutorError::from_error(&error)),
                DaemonExecRoute::CargoDiagnostics => self
                    .cargo_diagnostics
                    .as_mut()
                    .ok_or_else(DaemonToolExecutorError::unknown_tool)?
                    .execute(invocation)
                    .await
                    .map_err(|error| DaemonToolExecutorError::from_error(&error)),
            };
        }
        match name {
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
            name if WORKSPACE_READ_TOOL_NAMES.contains(&name) => self
                .workspace_read
                .as_mut()
                .ok_or_else(DaemonToolExecutorError::unknown_tool)?
                .execute(invocation)
                .await
                .map_err(|error| DaemonToolExecutorError::from_error(&error)),
            name if WORKSPACE_MUTATION_TOOL_NAMES.contains(&name) => self
                .workspace_mutation
                .as_mut()
                .ok_or_else(DaemonToolExecutorError::unknown_tool)?
                .execute(invocation)
                .await
                .map_err(|error| DaemonToolExecutorError::from_error(&error)),
            name if LOCAL_GIT_TOOL_NAMES.contains(&name) => self
                .local_git
                .as_mut()
                .ok_or_else(DaemonToolExecutorError::unknown_tool)?
                .execute(invocation)
                .await
                .map_err(|error| DaemonToolExecutorError::from_error(&error)),
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

#[cfg(test)]
mod tests {
    #[cfg(target_os = "linux")]
    use std::os::unix::ffi::OsStrExt;
    #[cfg(target_os = "linux")]
    use std::os::unix::fs::PermissionsExt;
    #[cfg(unix)]
    use std::os::unix::process::CommandExt;
    use std::{
        collections::BTreeSet,
        ffi::{OsStr, OsString},
        fmt, fs,
        io::{self, BufRead, BufReader, Cursor, ErrorKind, Read, Write},
        path::{Path, PathBuf},
        process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio},
        sync::{
            Mutex,
            mpsc::{self, Receiver, RecvTimeoutError, SyncSender},
        },
        thread::{self, JoinHandle},
        time::{Duration, Instant, SystemTime},
    };

    use expect_test::expect;
    use serde::{Deserialize, Deserializer, de::IgnoredAny};
    use signalbox_application::ToolCatalog;
    use signalbox_application::ToolInputSchema;
    use signalbox_domain::{ToolEffectClass, ToolPermissionDefault};
    use signalbox_model_runtime::{
        CancellationSignal, ConversationMessage, CredentialAccess, CredentialAccessError,
        CredentialReference, CredentialValue, ModelOperation, ModelRuntime, ModelSettings,
        Observation, ObservationSink, PreparationOutcome, RequestedTarget, ResolvedTarget,
    };
    use signalbox_model_runtime_claude_cli::{ClaudeCliConfig, ClaudeCliRuntime};

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

    const GIT_AUTHOR_NAME: &str = "Signalbox Daemon";
    const GIT_AUTHOR_EMAIL: &str = "signalbox@example.test";

    fn git_identity() -> GitIdentity {
        GitIdentity::try_new(GIT_AUTHOR_NAME, GIT_AUTHOR_EMAIL)
            .expect("fixture Git identity is valid")
    }

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

    #[derive(Clone, Copy, Debug)]
    struct OfflineSearchTransport;

    impl WebSearchTransport for OfflineSearchTransport {
        async fn search(
            &mut self,
            _request: signalbox_tools_web::WebSearchRequest,
            credential: &CredentialValue,
        ) -> signalbox_tools_web::WebSearchTransportOutcome {
            signalbox_tools_web::WebSearchTransportOutcome::failed(
                signalbox_tools_web::WebSearchTransportFailure::RequestFailed,
                credential,
            )
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

    const SYNTHETIC_OFFLINE_CREDENTIAL: &[u8] = b"offline-token";

    impl CredentialAccess for OfflineCredentials {
        async fn resolve(
            &self,
            _reference: &CredentialReference,
        ) -> Result<CredentialValue, CredentialAccessError> {
            Ok(CredentialValue::new(SYNTHETIC_OFFLINE_CREDENTIAL.to_vec()))
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

    const OFFLINE_CONVERSATION_PAGE_HAS_MORE: bool = false;

    impl ConversationIntrospectionPort for OfflineConversationPort {
        type Error = OfflineWriterError;

        async fn list_conversations(
            &mut self,
            _request: signalbox_tools_conversations::ConversationListRequest,
        ) -> Result<signalbox_tools_conversations::ConversationListPage, Self::Error> {
            Ok(signalbox_tools_conversations::ConversationListPage::new(
                Vec::new(),
                OFFLINE_CONVERSATION_PAGE_HAS_MORE,
            ))
        }

        async fn read_conversation(
            &mut self,
            _request: signalbox_tools_conversations::ConversationTranscriptRequest,
        ) -> Result<signalbox_tools_conversations::ConversationTranscriptRead, Self::Error>
        {
            Ok(signalbox_tools_conversations::ConversationTranscriptRead::NotFound)
        }

        async fn read_imported_conversation(
            &mut self,
            _request: signalbox_tools_conversations::ImportedTranscriptRequest,
        ) -> Result<Option<signalbox_tools_conversations::TranscriptPage>, Self::Error> {
            Ok(None)
        }
    }
    impl SessionPlanPort for OfflineConversationPort {
        type Error = OfflineWriterError;

        async fn append_plan_event(
            &mut self,
            _request: signalbox_tools_plan::PlanAppendRequest,
        ) -> Result<signalbox_tools_plan::PlanAppendOutcome, Self::Error> {
            Err(OfflineWriterError)
        }

        async fn read_plan(
            &mut self,
            _request: signalbox_tools_plan::PlanReadRequest,
        ) -> Result<signalbox_tools_plan::PlanReadPage, Self::Error> {
            Err(OfflineWriterError)
        }
    }

    fn definition_names(definitions: &[ToolDefinition]) -> Vec<&str> {
        definitions
            .iter()
            .map(|definition| definition.name().as_str())
            .collect()
    }

    #[track_caller]
    fn mapped_daemon_catalog(workspace: &Path) -> DaemonToolCatalog {
        git2::Repository::init(workspace).expect("fixture repository initializes");
        let web_fetch = WebFetchTool::try_new(OfflineTransport, WebFetchEgressPolicy::deny_all())
            .expect("offline web-fetch tool compiles");
        let web_search = WebSearchTool::try_new(
            OfflineCredentials,
            OfflineSearchTransport,
            WebSearchConfiguration::new(WebSearchProvider::Brave),
        )
        .expect("offline web-search tool compiles");
        let status =
            SessionStatusTool::try_new(OfflineWriter).expect("offline status tool compiles");
        let code_host = CodeHostTools::try_new(OfflineCredentials, OfflineCodeHostTransport)
            .expect("offline code-host tools compile");
        let github = GitHubTools::try_new(
            OfflineCredentials,
            OfflineGitHubTransport,
            GitHubEgressPolicy::github_api_only(),
        )
        .expect("offline GitHub tools compile");
        let workspace_read = WorkspaceReadTools::try_new(LocalWorkspaceFileSystem, workspace)
            .expect("workspace-read tools compile");
        let workspace_mutation =
            WorkspaceMutationTools::try_new(LocalWorkspaceFileSystem, workspace)
                .expect("workspace-mutation tools compile");
        let local_git = LocalGitTools::try_new(LocalWorkspaceFileSystem, workspace, git_identity())
            .expect("local-git tools compile");
        let process_runner = TokioProcessRunner::try_new(
            std::env::current_exe().expect("test executable path is available"),
        )
        .expect("test executable can stand in for the unused supervisor");
        let sandboxed_exec = SandboxedExecTool::try_new(process_runner.clone(), workspace)
            .expect("sandboxed-exec tool compiles");
        let unsandboxed_exec = UnsandboxedExecTool::try_new(process_runner.clone(), workspace)
            .expect("unsandboxed-exec tool compiles");
        let cargo_diagnostics = CargoDiagnosticsTool::try_new(process_runner, workspace)
            .expect("cargo-diagnostics tool compiles");
        let conversations = ConversationTools::try_new(OfflineConversationPort)
            .expect("offline conversation tools compile");
        let plan = PlanTools::try_new(OfflineConversationPort).expect("offline plan tools compile");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("fixture runtime builds");
        let _runtime_guard = runtime.enter();
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy(SYNTHETIC_GOAL_DATABASE_URL)
            .expect("synthetic lazy goal pool is valid");
        let goal = GoalDeclarationTool::try_new(pool).expect("static goal tool compiles");

        DaemonTools::try_new_with_tools(
            || SystemTime::UNIX_EPOCH,
            ComposedToolFamilies {
                web_fetch,
                web_search,
                status,
                code_host,
                github: Some(github),
                workspace_read: Some(workspace_read),
                workspace_mutation: Some(workspace_mutation),
                local_git: Some(local_git),
                sandboxed_exec: Some(sandboxed_exec),
                unsandboxed_exec: Some(unsandboxed_exec),
                cargo_diagnostics: Some(cargo_diagnostics),
                conversations: Some(conversations),
                plan,
                delegation: SessionDelegationTools::try_new(
                    DaemonSessionDelegationPort::unavailable(),
                )
                .expect("offline session-delegation tools compile"),
                goal: Some(goal),
            },
        )
        .expect("static daemon tools compile")
        .into_parts()
        .0
    }

    #[test]
    fn production_constructor_matches_the_complete_mapped_catalog() {
        let expected_workspace = tempfile::tempdir().expect("expected workspace exists");
        let expected_catalog = mapped_daemon_catalog(expected_workspace.path());
        let expected_definitions = expected_catalog.definitions();
        let workspace = tempfile::tempdir().expect("production workspace exists");
        git2::Repository::init(workspace.path()).expect("production repository initializes");
        let support = tempfile::tempdir().expect("credential fixture root exists");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("production fixture runtime builds");
        let _runtime_guard = runtime.enter();
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy(SYNTHETIC_GOAL_DATABASE_URL)
            .expect("synthetic production pool is valid");
        let tools = DaemonTools::try_new_production(
            || SystemTime::UNIX_EPOCH,
            pool,
            MappedDaemonCredentialInputs {
                web_search: FileCredentialAccess::new(
                    support.path().join(SYNTHETIC_WEB_SEARCH_CREDENTIAL_PATH),
                    CredentialReference::new(SYNTHETIC_WEB_SEARCH_CREDENTIAL_REFERENCE),
                ),
                code_host: FileCredentialAccess::new(
                    support.path().join(SYNTHETIC_CODE_HOST_CREDENTIAL_PATH),
                    CredentialReference::new(SYNTHETIC_CODE_HOST_CREDENTIAL_REFERENCE),
                ),
                github: FileCredentialAccess::new(
                    support.path().join(SYNTHETIC_GITHUB_CREDENTIAL_PATH),
                    CredentialReference::new(SYNTHETIC_GITHUB_CREDENTIAL_REFERENCE),
                ),
            },
            GitHubCodeHostTransport::try_new().expect("offline code-host transport constructs"),
            GitHubEgressPolicy::github_api_only(),
            workspace.path(),
            git_identity(),
            &std::env::current_exe().expect("test executable path is available"),
            WebFetchEgressPolicy::deny_all(),
        )
        .expect("production daemon tools compile");
        let (catalog, _executor) = tools.into_parts();
        let actual_definitions = catalog.definitions();
        let actual_names = definition_names(&actual_definitions);

        assert_eq!(actual_definitions, expected_definitions);
        assert!(actual_names.contains(&GOAL_DECLARE_NAME));
    }

    /// Renders the bridge catalog document from the daemon registry through the
    /// production projection and Claude adapter translation used by prepared
    /// support files.
    ///
    /// Routing the bridge's input through `runtime_tool_definitions` means a
    /// projection or Claude translation that drops or alters a daemon tool
    /// changes what the bridge is given, while `expected_bridge_tools` derives
    /// the expectation straight from the registry. The listing assertion
    /// therefore classifies the daemon-to-Claude-to-bridge path rather than
    /// comparing one helper with itself.
    #[cfg(target_os = "linux")]
    #[track_caller]
    fn bridge_catalog(definitions: &[ToolDefinition]) -> CapturedBridgeCatalog {
        let projected = signalbox_model_provider_runtime::runtime_tool_definitions(definitions)
            .expect("daemon tool schemas project into runtime definitions");
        let executable = ensure_claude_mcp_bridge_executable();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("catalog capture runtime constructs");
        let catalog = runtime.block_on(capture_prepared_claude_catalog(projected, &executable));
        CapturedBridgeCatalog {
            catalog,
            executable,
        }
    }

    #[cfg(target_os = "linux")]
    struct CapturedBridgeCatalog {
        catalog: Vec<u8>,
        executable: PathBuf,
    }

    #[cfg(target_os = "linux")]
    async fn capture_prepared_claude_catalog(
        tools: Vec<signalbox_model_runtime::ToolDefinition>,
        bridge: &Path,
    ) -> Vec<u8> {
        let workspace = tempfile::tempdir().expect("catalog capture workspace exists");
        let executable = workspace.path().join(CLAUDE_CATALOG_CAPTURE_EXECUTABLE);
        fs::write(&executable, CLAUDE_CATALOG_CAPTURE_SCRIPT)
            .expect("catalog capture executable is written");
        let mut permissions = fs::metadata(&executable)
            .expect("catalog capture executable metadata is available")
            .permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&executable, permissions)
            .expect("catalog capture executable is private and executable");
        fs::write(
            workspace.path().join(CLAUDE_EXPECTED_BRIDGE_PATH_FILENAME),
            bridge
                .to_str()
                .expect("catalog capture bridge path is valid UTF-8"),
        )
        .expect("expected catalog bridge path is written");
        let credential = CredentialReference::new(SYNTHETIC_CLAUDE_CREDENTIAL_REFERENCE);
        let mut config =
            ClaudeCliConfig::new(&executable, bridge, workspace.path(), credential.clone());
        config.exchange_timeout = BRIDGE_RESPONSE_TIMEOUT;
        let runtime = ClaudeCliRuntime::new(config)
            .expect("offline Claude catalog capture runtime constructs");
        let mut operation = ModelOperation::new(
            (),
            credential,
            RequestedTarget::new(SYNTHETIC_CLAUDE_SELECTION),
            ResolvedTarget::new(SYNTHETIC_CLAUDE_MODEL),
            vec![ConversationMessage::user_text(SYNTHETIC_CLAUDE_PROMPT)],
            ModelSettings::new(SYNTHETIC_CLAUDE_MAX_OUTPUT_TOKENS),
        );
        operation.tools = tools;
        let prepared = match runtime
            .prepare(operation, CancellationSignal::never())
            .await
        {
            PreparationOutcome::Prepared(prepared) => prepared,
            PreparationOutcome::Cancelled { .. } => panic!("catalog capture was cancelled"),
            PreparationOutcome::Failed { failure, .. } => {
                panic!("Claude catalog translation failed: {failure:?}")
            }
            PreparationOutcome::Defect { defect, .. } => {
                panic!("Claude catalog preparation found a defect: {defect:?}")
            }
        };
        let mut observations = DiscardClaudeCatalogObservations;
        let _report = runtime
            .execute(prepared, &mut observations, CancellationSignal::never())
            .await;
        let captured_config = fs::read(workspace.path().join(CLAUDE_CAPTURED_CONFIG_FILENAME))
            .expect("the fake CLI captured the prepared MCP config");
        let captured_paths = claude_catalog_paths_from_config(&captured_config, bridge)
            .expect("the captured MCP config names the bridge catalog and readiness marker");
        let exercised_ready = fs::read_to_string(
            workspace
                .path()
                .join(CLAUDE_CAPTURED_READY_EXERCISE_FILENAME),
        )
        .expect("the fake CLI exercised the configured readiness path");
        assert_eq!(Path::new(&exercised_ready), captured_paths.ready);
        assert_eq!(
            captured_paths.catalog,
            PathBuf::from(
                fs::read_to_string(workspace.path().join(CLAUDE_CAPTURED_CATALOG_PATH_FILENAME))
                    .expect("the fake CLI captured the configured catalog path"),
            )
        );
        fs::read(workspace.path().join(CLAUDE_CAPTURED_CATALOG_FILENAME))
            .expect("the fake CLI captured the prepared Claude catalog")
    }

    #[cfg(target_os = "linux")]
    struct DiscardClaudeCatalogObservations;

    #[cfg(target_os = "linux")]
    impl ObservationSink<()> for DiscardClaudeCatalogObservations {
        fn observe(&mut self, _observation: Observation<()>) {}
    }

    #[cfg(target_os = "linux")]
    #[derive(Debug, Eq, PartialEq)]
    struct CapturedClaudeMcpPaths {
        catalog: PathBuf,
        ready: PathBuf,
    }

    #[cfg(target_os = "linux")]
    fn claude_catalog_paths_from_config(
        config: &[u8],
        expected_bridge: &Path,
    ) -> Option<CapturedClaudeMcpPaths> {
        let config: serde_json::Value = serde_json::from_slice(config).ok()?;
        let servers = config.get("mcpServers")?.as_object()?;
        let server = (servers.len() == 1)
            .then(|| servers.get(CLAUDE_MCP_SERVER_NAME))
            .flatten()?;
        let transport = server.get("type")?.as_str()?;
        let command = Path::new(server.get("command")?.as_str()?);
        let arguments = server.get("args")?.as_array()?;
        if transport != CLAUDE_MCP_STDIO_TRANSPORT
            || command != expected_bridge
            || arguments.len() != 3
            || arguments.first()?.as_str()? != CLAUDE_MCP_BRIDGE_SERVE_OPTION
        {
            return None;
        }
        Some(CapturedClaudeMcpPaths {
            catalog: PathBuf::from(arguments.get(1)?.as_str()?),
            ready: PathBuf::from(arguments.get(2)?.as_str()?),
        })
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct ComparableBridgeTool {
        name: String,
        description: String,
        input_schema: ToolInputSchema,
    }

    #[derive(Deserialize)]
    struct ListedBridgeResponse {
        jsonrpc: String,
        id: u64,
        result: Option<ListedBridgeResult>,
        #[serde(
            default,
            rename = "error",
            deserialize_with = "deserialize_present_json_member"
        )]
        error_present: bool,
    }

    fn deserialize_present_json_member<'de, DeserializerType>(
        deserializer: DeserializerType,
    ) -> Result<bool, DeserializerType::Error>
    where
        DeserializerType: Deserializer<'de>,
    {
        IgnoredAny::deserialize(deserializer).map(|_| true)
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct ListedBridgeResult {
        tools: Vec<ListedBridgeTool>,
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct ListedBridgeTool {
        name: String,
        description: String,
        #[serde(rename = "inputSchema")]
        input_schema: Box<serde_json::value::RawValue>,
    }

    impl ListedBridgeResponse {
        fn into_tools(self, request_id: u64) -> Option<Vec<ComparableBridgeTool>> {
            let result = (self.jsonrpc == MCP_JSON_RPC_VERSION
                && self.id == request_id
                && !self.error_present)
                .then_some(self.result?)?;
            result
                .tools
                .into_iter()
                .map(ListedBridgeTool::into_comparable)
                .collect()
        }
    }

    impl ListedBridgeTool {
        fn into_comparable(self) -> Option<ComparableBridgeTool> {
            Some(ComparableBridgeTool {
                name: self.name,
                description: self.description,
                input_schema: ToolInputSchema::try_new(self.input_schema.get().to_owned()).ok()?,
            })
        }
    }

    /// The MCP tool listing the daemon registry itself declares.
    ///
    /// This reads the application definitions directly, so it is an
    /// independent source from the projected document the bridge is started
    /// with.
    #[track_caller]
    fn expected_bridge_tools(definitions: &[ToolDefinition]) -> Vec<ComparableBridgeTool> {
        definitions
            .iter()
            .map(|definition| ComparableBridgeTool {
                name: definition.name().as_str().to_owned(),
                description: definition.description().to_owned(),
                input_schema: definition.input_schema().clone(),
            })
            .collect()
    }

    const SYNTHETIC_BRIDGE_TOOL_NAME: &str = "synthetic_bridge_tool";
    const SYNTHETIC_BRIDGE_TOOL_DESCRIPTION: &str = "Projects a synthetic bridge tool.";
    const SYNTHETIC_BRIDGE_TOOL_SCHEMA: &str =
        r#"{"properties":{"value":{"type":"string"}},"required":["value"],"type":"object"}"#;
    const SYNTHETIC_DEEP_BRIDGE_SCHEMA_DEPTH: usize = 512;
    const SYNTHETIC_UNMODELED_BRIDGE_TOOL_TITLE: &str = "Synthetic unmodeled title";
    const SYNTHETIC_MCP_NEXT_CURSOR: &str = "synthetic-next-page";
    const SYNTHETIC_MCP_IGNORED_ARGUMENT: &str = "ready path";

    fn synthetic_deep_bridge_tool_schema() -> String {
        let mut schema = String::from(r#"{"type":"string"}"#);
        for _ in 0..SYNTHETIC_DEEP_BRIDGE_SCHEMA_DEPTH {
            schema = format!(r#"{{"properties":{{"nested":{schema}}},"type":"object"}}"#);
        }
        schema
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn captured_catalog_path_uses_semantic_mcp_configuration() {
        let config = serde_json::to_string_pretty(&serde_json::json!({
            "mcpServers": {
                (CLAUDE_MCP_SERVER_NAME): {
                    "type": CLAUDE_MCP_STDIO_TRANSPORT,
                    "command": SYNTHETIC_MCP_BRIDGE_PATH,
                    "args": [
                        CLAUDE_MCP_BRIDGE_SERVE_OPTION,
                        SYNTHETIC_CAPTURED_CATALOG_PATH,
                        SYNTHETIC_MCP_IGNORED_ARGUMENT,
                    ]
                }
            }
        }))
        .expect("synthetic MCP config serializes");

        assert_eq!(
            claude_catalog_paths_from_config(
                config.as_bytes(),
                Path::new(SYNTHETIC_MCP_BRIDGE_PATH),
            ),
            Some(CapturedClaudeMcpPaths {
                catalog: PathBuf::from(SYNTHETIC_CAPTURED_CATALOG_PATH),
                ready: PathBuf::from(SYNTHETIC_MCP_IGNORED_ARGUMENT),
            })
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn captured_catalog_path_rejects_a_different_mcp_server_name() {
        let config = serde_json::to_vec(&serde_json::json!({
            "mcpServers": {
                "different_server": {
                    "type": CLAUDE_MCP_STDIO_TRANSPORT,
                    "command": SYNTHETIC_MCP_BRIDGE_PATH,
                    "args": [
                        CLAUDE_MCP_BRIDGE_SERVE_OPTION,
                        SYNTHETIC_CAPTURED_CATALOG_PATH,
                        SYNTHETIC_MCP_IGNORED_ARGUMENT,
                    ]
                }
            }
        }))
        .expect("synthetic MCP config serializes");

        assert_eq!(
            claude_catalog_paths_from_config(&config, Path::new(SYNTHETIC_MCP_BRIDGE_PATH)),
            None
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn captured_catalog_path_rejects_a_different_bridge_command() {
        let config = serde_json::to_vec(&serde_json::json!({
            "mcpServers": {
                (CLAUDE_MCP_SERVER_NAME): {
                    "type": CLAUDE_MCP_STDIO_TRANSPORT,
                    "command": "different-claude-mcp-bridge",
                    "args": [
                        CLAUDE_MCP_BRIDGE_SERVE_OPTION,
                        SYNTHETIC_CAPTURED_CATALOG_PATH,
                        SYNTHETIC_MCP_IGNORED_ARGUMENT,
                    ]
                }
            }
        }))
        .expect("synthetic MCP config serializes");

        assert_eq!(
            claude_catalog_paths_from_config(&config, Path::new(SYNTHETIC_MCP_BRIDGE_PATH)),
            None
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn captured_catalog_path_rejects_a_non_stdio_transport() {
        let config = serde_json::to_vec(&serde_json::json!({
            "mcpServers": {
                (CLAUDE_MCP_SERVER_NAME): {
                    "type": "http",
                    "command": SYNTHETIC_MCP_BRIDGE_PATH,
                    "args": [
                        CLAUDE_MCP_BRIDGE_SERVE_OPTION,
                        SYNTHETIC_CAPTURED_CATALOG_PATH,
                        SYNTHETIC_MCP_IGNORED_ARGUMENT,
                    ]
                }
            }
        }))
        .expect("synthetic MCP config serializes");

        assert_eq!(
            claude_catalog_paths_from_config(&config, Path::new(SYNTHETIC_MCP_BRIDGE_PATH)),
            None
        );
    }

    struct BridgeArtifactSelection {
        profile: OsString,
        target: Option<OsString>,
        target_dir: PathBuf,
    }

    #[derive(Debug, Eq, PartialEq)]
    struct CargoTestInvocation {
        profile: OsString,
        config_overrides: Vec<OsString>,
        unstable_flags: Vec<OsString>,
        ignore_rust_version: bool,
        invocation_directory: PathBuf,
    }

    struct BridgeBuildLocation<'a> {
        invocation_directory: &'a Path,
        workspace: &'a Path,
    }

    struct ConfiguredCargoTargetDirInput<'a> {
        current_executable: &'a Path,
        configured: &'a Path,
        known_targets: &'a BTreeSet<OsString>,
    }

    struct ConfiguredCargoTargetDirLookup<'a> {
        current_executable: &'a Path,
        invocation_directory: &'a Path,
        known_targets: &'a BTreeSet<OsString>,
    }

    struct RelativeConfiguredTargetDirInput<'a> {
        current_executable: &'a Path,
        configured: &'a Path,
        invocation_directory: &'a Path,
        known_targets: &'a BTreeSet<OsString>,
    }

    struct AdmittedConfiguredTargetDirInput<'a> {
        current_executable: &'a Path,
        configured_target_dir: &'a Path,
        known_targets: &'a BTreeSet<OsString>,
    }

    struct ConfiguredTargetMatchInput<'a> {
        current_executable: &'a Path,
        candidate: &'a Path,
        known_targets: &'a BTreeSet<OsString>,
    }

    struct DefaultTargetRecognition<'a> {
        current_executable: &'a Path,
        configured_target_dir: Option<&'a Path>,
        default_target_dir: &'a Path,
        artifact_target_dir: Option<&'a Path>,
        known_targets: &'a BTreeSet<OsString>,
    }

    struct BridgeArtifactSelectionInput<'a> {
        current_executable: &'a Path,
        debug_profile: &'a str,
        configured_target_dir: Option<&'a Path>,
        default_target_dir: &'a Path,
        artifact_target_dir: Option<&'a Path>,
        known_targets: &'a BTreeSet<OsString>,
    }

    const CLAUDE_MCP_BRIDGE_BINARY: &str = "signalbox-claude-mcp-bridge";
    const CLAUDE_MCP_SERVER_NAME: &str = "signalbox_tools";
    const CLAUDE_MCP_BRIDGE_SERVE_OPTION: &str = "--serve";
    const CLAUDE_MCP_BRIDGE_WAIT_READY_OPTION: &str = "--wait-ready";
    const CARGO_TARGET_DIRECTORY_MARKER_FILENAME: &str = "CACHEDIR.TAG";
    const EMPTY_CARGO_TARGET_DIRECTORY_MARKER: &[u8] = b"";
    const CARGO_TEST_PROFILE: &str = "test";
    const CARGO_DEV_PROFILE: &str = "dev";
    const CARGO_BENCH_PROFILE: &str = "bench";
    const CARGO_RELEASE_PROFILE: &str = "release";
    const CARGO_DEBUG_PROFILE_DIRECTORY: &str = "debug";
    const CARGO_TEST_SUBCOMMAND: &str = "test";
    const CARGO_TEST_SUBCOMMAND_ALIAS: &str = "t";
    const CARGO_PROGRAM_STEM: &str = "cargo";
    const CARGO_PROFILE_OPTION: &str = "--profile";
    const CARGO_PROFILE_OPTION_PREFIX: &str = "--profile=";
    const CARGO_CONFIG_OPTION: &str = "--config";
    const CARGO_CONFIG_OPTION_PREFIX: &str = "--config=";
    const CARGO_MANIFEST_PATH_OPTION: &str = "--manifest-path";
    const CARGO_MANIFEST_FILENAME: &str = "Cargo.toml";
    const CARGO_COLOR_OPTION: &str = "--color";
    const CARGO_CHANGE_DIRECTORY_OPTION: &str = "-C";
    const CARGO_UNSTABLE_OPTION: &str = "-Z";
    const SYNTHETIC_CARGO_COLOR_OPTION_VALUE: &str = "always";
    const SYNTHETIC_CARGO_CHANGE_DIRECTORY_OPTION_VALUE: &str = "synthetic-workspace";
    const SYNTHETIC_CARGO_UNSTABLE_OPTION_VALUE: &str = "unstable-options";
    const SYNTHETIC_POST_SUBCOMMAND_UNSTABLE_OPTION_VALUE: &str = "profile-rustflags";
    const CARGO_RELEASE_OPTION: &str = "--release";
    const CARGO_RELEASE_SHORT_OPTION: &str = "-r";
    const CARGO_IGNORE_RUST_VERSION_OPTION: &str = "--ignore-rust-version";
    const MCP_JSON_RPC_VERSION: &str = "2.0";
    const MCP_INVALID_PARAMS_ERROR_CODE: i64 = -32602;
    const SYNTHETIC_WRONG_JSON_RPC_VERSION: &str = "1.0";
    #[cfg(target_os = "linux")]
    const PROC_FILESYSTEM_ROOT: &str = "/proc";
    #[cfg(target_os = "linux")]
    const PROC_COMMAND_LINE_FILENAME: &str = "cmdline";
    #[cfg(target_os = "linux")]
    const PROC_PROCESS_STAT_FILENAME: &str = "stat";
    #[cfg(target_os = "linux")]
    const PROC_WORKING_DIRECTORY_FILENAME: &str = "cwd";
    #[cfg(target_os = "linux")]
    const MAX_CARGO_COMMAND_LINE_BYTES: u64 = 64 * 1024;
    const SYNTHETIC_GOAL_DATABASE_URL: &str =
        "postgresql://signalbox:synthetic@127.0.0.1/signalbox";
    const SYNTHETIC_WEB_SEARCH_CREDENTIAL_PATH: &str = "web-search";
    const SYNTHETIC_WEB_SEARCH_CREDENTIAL_REFERENCE: &str = "synthetic-web-search";
    const SYNTHETIC_CODE_HOST_CREDENTIAL_PATH: &str = "code-host";
    const SYNTHETIC_CODE_HOST_CREDENTIAL_REFERENCE: &str = "synthetic-code-host";
    const SYNTHETIC_GITHUB_CREDENTIAL_PATH: &str = "github";
    const SYNTHETIC_GITHUB_CREDENTIAL_REFERENCE: &str = "synthetic-github";
    const SYNTHETIC_CLAUDE_CREDENTIAL_REFERENCE: &str = "synthetic-claude";
    const SYNTHETIC_CLAUDE_SELECTION: &str = "synthetic-claude-selection";
    const SYNTHETIC_CLAUDE_MODEL: &str = "synthetic-claude-model";
    const SYNTHETIC_CLAUDE_PROMPT: &str = "Capture the prepared MCP catalog";
    const SYNTHETIC_CLAUDE_MAX_OUTPUT_TOKENS: u32 = 256;
    #[cfg(target_os = "linux")]
    const CLAUDE_CATALOG_CAPTURE_EXECUTABLE: &str = "capture-claude-catalog";
    #[cfg(target_os = "linux")]
    const CLAUDE_CAPTURED_CONFIG_FILENAME: &str = "captured-mcp-config.json";
    #[cfg(target_os = "linux")]
    const CLAUDE_CAPTURED_CATALOG_FILENAME: &str = "captured-mcp-catalog.json";
    #[cfg(target_os = "linux")]
    const CLAUDE_CAPTURED_CATALOG_PATH_FILENAME: &str = "captured-mcp-catalog-path";
    #[cfg(target_os = "linux")]
    const CLAUDE_CAPTURED_READY_EXERCISE_FILENAME: &str = "captured-mcp-ready-exercised";
    #[cfg(target_os = "linux")]
    const CLAUDE_EXPECTED_BRIDGE_PATH_FILENAME: &str = "expected-claude-mcp-bridge-path";
    #[cfg(target_os = "linux")]
    const SYNTHETIC_CAPTURED_CATALOG_PATH: &str = "catalog with a \"quote\".json";
    #[cfg(target_os = "linux")]
    const SYNTHETIC_MCP_BRIDGE_PATH: &str = "synthetic-claude-mcp-bridge";
    #[cfg(target_os = "linux")]
    const CLAUDE_MCP_STDIO_TRANSPORT: &str = "stdio";
    #[cfg(target_os = "linux")]
    #[cfg(target_os = "linux")]
    const CLAUDE_CATALOG_CAPTURE_SCRIPT: &str = r#"#!/bin/sh
set -eu
mcp_config=
settings=
while [ "$#" -gt 0 ]; do
  if [ "$1" = "--mcp-config" ]; then
    shift
    mcp_config=$1
  elif [ "$1" = "--settings" ]; then
    shift
    settings=$1
  fi
  shift
done
test -n "$mcp_config"
test -n "$settings"
capture_dir=${0%/*}
cp "$mcp_config" "$capture_dir/captured-mcp-config.json"
python3 -c 'import json, pathlib, shlex, shutil, subprocess, sys
with open(sys.argv[1], encoding="utf-8") as source:
    servers = json.load(source)["mcpServers"]
assert len(servers) == 1
server = servers["signalbox_tools"]
assert server["type"] == "stdio"
arguments = server["args"]
assert len(arguments) == 3 and arguments[0] == "--serve"
with open(sys.argv[3], encoding="utf-8") as source:
    settings = json.load(source)
hook = settings["hooks"]["SessionStart"][0]["hooks"][0]
assert hook["type"] == "command"
expected_bridge = pathlib.Path(sys.argv[6]).read_text(encoding="utf-8")
assert server["command"] == expected_bridge
hook_arguments = shlex.split(hook["command"])
expected_hook_arguments = [expected_bridge, "--wait-ready", arguments[2]]
assert hook_arguments == expected_hook_arguments or hook_arguments == ["exec", *expected_hook_arguments]
shutil.copyfile(arguments[1], sys.argv[2])
pathlib.Path(sys.argv[4]).write_text(arguments[1], encoding="utf-8")
bridge = subprocess.Popen(
    [server["command"], *arguments],
    stdin=subprocess.PIPE,
    stdout=subprocess.PIPE,
    stderr=subprocess.DEVNULL,
    text=True,
)
waiter = subprocess.Popen(
    hook["command"], shell=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL
)
try:
    initialize = {
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": {"name": "catalog-capture", "version": "1"},
        },
    }
    bridge.stdin.write(json.dumps(initialize) + "\n")
    bridge.stdin.flush()
    assert json.loads(bridge.stdout.readline())["id"] == 1
    assert waiter.poll() is None
    bridge.stdin.write(json.dumps({"jsonrpc": "2.0", "method": "notifications/initialized"}) + "\n")
    bridge.stdin.write(json.dumps({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}) + "\n")
    bridge.stdin.flush()
    assert json.loads(bridge.stdout.readline())["id"] == 2
    assert waiter.wait(timeout=12) == 0
    assert pathlib.Path(arguments[2]).is_file()
    pathlib.Path(sys.argv[5]).write_text(arguments[2], encoding="utf-8")
    bridge.stdin.close()
    assert bridge.wait(timeout=12) == 0
finally:
    if waiter.poll() is None:
        waiter.terminate()
    if bridge.poll() is None:
        bridge.terminate()' \
  "$mcp_config" "$capture_dir/captured-mcp-catalog.json" "$settings" \
  "$capture_dir/captured-mcp-catalog-path" "$capture_dir/captured-mcp-ready-exercised" \
  "$capture_dir/expected-claude-mcp-bridge-path"
"#;

    #[cfg(target_os = "linux")]
    #[track_caller]
    fn claude_mcp_bridge_artifact_selection(
        invocation: &CargoTestInvocation,
    ) -> BridgeArtifactSelection {
        let current = std::env::current_exe().expect("test executable path is available");
        let known_targets = rustc_target_names(&invocation.invocation_directory);
        let configured_target_dir = configured_cargo_target_dir(ConfiguredCargoTargetDirLookup {
            current_executable: &current,
            invocation_directory: &invocation.invocation_directory,
            known_targets: &known_targets,
        });
        let artifact_target_dir = cargo_target_dir_from_artifact(&current);
        let default_target_dir = bridge_build_target_dir(
            BridgeBuildTargetCandidates {
                configured: configured_target_dir.as_deref(),
                executable_artifact: artifact_target_dir.as_deref(),
            },
            cargo_metadata_target_dir,
        );
        reject_unrecognized_default_target(DefaultTargetRecognition {
            current_executable: &current,
            configured_target_dir: configured_target_dir.as_deref(),
            default_target_dir: &default_target_dir,
            artifact_target_dir: artifact_target_dir.as_deref(),
            known_targets: &known_targets,
        });
        claude_mcp_bridge_artifact_selection_for(BridgeArtifactSelectionInput {
            current_executable: &current,
            debug_profile: invocation
                .profile
                .to_str()
                .expect("Cargo profile names are valid UTF-8"),
            configured_target_dir: configured_target_dir.as_deref(),
            default_target_dir: &default_target_dir,
            artifact_target_dir: artifact_target_dir.as_deref(),
            known_targets: &known_targets,
        })
    }

    #[cfg(target_os = "linux")]
    #[track_caller]
    fn current_cargo_test_invocation() -> CargoTestInvocation {
        parent_cargo_test_invocation().unwrap_or_else(cargo_test_invocation_from_running_artifact)
    }

    #[cfg(target_os = "linux")]
    fn parent_cargo_test_invocation() -> Option<CargoTestInvocation> {
        let parent = rustix::process::getppid()?;
        let parent_process_directory =
            Path::new(PROC_FILESYSTEM_ROOT).join(parent.as_raw_nonzero().get().to_string());
        let command_line_path = parent_process_directory.join(PROC_COMMAND_LINE_FILENAME);
        let mut command_line = Vec::new();
        fs::File::open(command_line_path)
            .ok()?
            .take(MAX_CARGO_COMMAND_LINE_BYTES + 1)
            .read_to_end(&mut command_line)
            .ok()?;
        (command_line.len() as u64 <= MAX_CARGO_COMMAND_LINE_BYTES).then_some(())?;
        let arguments = command_line
            .split(|byte| *byte == 0)
            .filter(|argument| !argument.is_empty())
            .map(OsStr::from_bytes)
            .collect::<Vec<_>>();
        let invocation_directory = cargo_invocation_directory(&parent_process_directory).ok()?;
        cargo_test_invocation_from_arguments(&arguments, &invocation_directory)
    }

    #[cfg(target_os = "linux")]
    #[track_caller]
    fn cargo_test_invocation_from_running_artifact() -> CargoTestInvocation {
        let current = std::env::current_exe().expect("test executable path is available");
        let invocation_directory =
            std::env::current_dir().expect("direct test invocation directory is available");
        cargo_test_invocation_from_artifact(CargoTestArtifactInvocation {
            current_executable: &current,
            invocation_directory: &invocation_directory,
        })
        .expect("direct Cargo test artifacts must retain an unambiguous profile directory")
    }

    struct CargoTestArtifactInvocation<'a> {
        current_executable: &'a Path,
        invocation_directory: &'a Path,
    }

    fn cargo_test_invocation_from_artifact(
        input: CargoTestArtifactInvocation<'_>,
    ) -> Option<CargoTestInvocation> {
        let profile_directory = input
            .current_executable
            .parent()
            .and_then(Path::parent)
            .expect("test executable is under a Cargo profile directory");
        let profile_name = profile_directory
            .file_name()
            .expect("Cargo profile directory has a name");
        let profile = match profile_name.to_str()? {
            CARGO_DEBUG_PROFILE_DIRECTORY | CARGO_RELEASE_PROFILE => return None,
            profile => OsString::from(profile),
        };
        Some(CargoTestInvocation {
            profile,
            config_overrides: Vec::new(),
            unstable_flags: Vec::new(),
            ignore_rust_version: false,
            invocation_directory: input.invocation_directory.to_path_buf(),
        })
    }

    #[cfg(target_os = "linux")]
    fn cargo_invocation_directory(parent_process_directory: &Path) -> io::Result<PathBuf> {
        fs::read_link(parent_process_directory.join(PROC_WORKING_DIRECTORY_FILENAME))
    }

    fn cargo_test_profile_from_arguments(arguments: &[&OsStr]) -> Option<OsString> {
        cargo_test_invocation_from_arguments(arguments, Path::new("."))
            .map(|invocation| invocation.profile)
    }

    fn cargo_test_invocation_from_arguments(
        arguments: &[&OsStr],
        invocation_directory: &Path,
    ) -> Option<CargoTestInvocation> {
        if Path::new(arguments.first()?).file_stem()? != OsStr::new(CARGO_PROGRAM_STEM) {
            return None;
        }
        let mut profile = None;
        let mut config_overrides = Vec::new();
        let mut unstable_flags = Vec::new();
        let mut ignore_rust_version = false;
        let mut found_test_subcommand = false;
        let mut index = 1;
        while let Some(argument) = arguments.get(index).copied() {
            if argument == OsStr::new(CARGO_CONFIG_OPTION) {
                let config = arguments.get(index + 1).copied()?;
                config_overrides.push(normalized_cargo_config_override(
                    config,
                    invocation_directory,
                )?);
                index += 2;
                continue;
            }
            if let Some(config) = argument
                .to_str()
                .and_then(|argument| argument.strip_prefix(CARGO_CONFIG_OPTION_PREFIX))
            {
                config_overrides.push(normalized_cargo_config_override(
                    OsStr::new(config),
                    invocation_directory,
                )?);
                index += 1;
                continue;
            }
            if argument == OsStr::new(CARGO_UNSTABLE_OPTION) {
                unstable_flags.push(arguments.get(index + 1)?.to_os_string());
                index += 2;
                continue;
            }
            if let Some(unstable_flag) = argument
                .to_str()
                .and_then(|argument| argument.strip_prefix(CARGO_UNSTABLE_OPTION))
                .filter(|unstable_flag| !unstable_flag.is_empty())
            {
                unstable_flags.push(OsString::from(unstable_flag));
                index += 1;
                continue;
            }
            if !found_test_subcommand {
                if matches!(
                    argument.to_str(),
                    Some(CARGO_TEST_SUBCOMMAND | CARGO_TEST_SUBCOMMAND_ALIAS)
                ) {
                    found_test_subcommand = true;
                } else if cargo_global_option_takes_value(argument) {
                    arguments.get(index + 1)?;
                    index += 2;
                    continue;
                } else if !argument.as_encoded_bytes().starts_with(b"+")
                    && !argument.as_encoded_bytes().starts_with(b"-")
                {
                    return None;
                }
                index += 1;
                continue;
            }
            if argument == OsStr::new(CARGO_PROFILE_OPTION) {
                profile = Some(arguments.get(index + 1)?.to_os_string());
                index += 2;
                continue;
            }
            if let Some(argument_profile) = argument
                .to_str()
                .and_then(|argument| argument.strip_prefix(CARGO_PROFILE_OPTION_PREFIX))
            {
                profile = Some(OsString::from(argument_profile));
                index += 1;
                continue;
            }
            if matches!(
                argument.to_str(),
                Some(CARGO_RELEASE_OPTION | CARGO_RELEASE_SHORT_OPTION)
            ) {
                profile = Some(OsString::from(CARGO_RELEASE_PROFILE));
            }
            if argument == OsStr::new(CARGO_IGNORE_RUST_VERSION_OPTION) {
                ignore_rust_version = true;
            }
            index += 1;
        }
        found_test_subcommand.then(|| CargoTestInvocation {
            profile: profile.unwrap_or_else(|| OsString::from(CARGO_TEST_PROFILE)),
            config_overrides,
            unstable_flags,
            ignore_rust_version,
            invocation_directory: invocation_directory.to_path_buf(),
        })
    }

    fn cargo_global_option_takes_value(argument: &OsStr) -> bool {
        matches!(
            argument.to_str(),
            Some(CARGO_COLOR_OPTION | CARGO_CHANGE_DIRECTORY_OPTION)
        )
    }

    fn apply_cargo_unstable_flags(command: &mut Command, flags: &[OsString]) {
        for flag in flags {
            command.arg(CARGO_UNSTABLE_OPTION).arg(flag);
        }
    }

    fn normalized_cargo_config_override(
        config: &OsStr,
        invocation_directory: &Path,
    ) -> Option<OsString> {
        (!config.is_empty()).then(|| {
            if config.as_encoded_bytes().contains(&b'=') || Path::new(config).is_absolute() {
                config.to_os_string()
            } else {
                invocation_directory.join(config).into_os_string()
            }
        })
    }

    #[track_caller]
    fn configured_cargo_target_dir(input: ConfiguredCargoTargetDirLookup<'_>) -> Option<PathBuf> {
        let configured = PathBuf::from(std::env::var_os("CARGO_TARGET_DIR")?);
        let configured = if configured.is_absolute() {
            configured
        } else {
            resolved_or_executable_configured_target_dir(RelativeConfiguredTargetDirInput {
                current_executable: input.current_executable,
                configured: &configured,
                invocation_directory: input.invocation_directory,
                known_targets: input.known_targets,
            })
        };
        let configured = configured_cargo_target_dir_for(ConfiguredCargoTargetDirInput {
            current_executable: input.current_executable,
            configured: &configured,
            known_targets: input.known_targets,
        });
        admitted_configured_target_dir(AdmittedConfiguredTargetDirInput {
            current_executable: input.current_executable,
            configured_target_dir: &configured,
            known_targets: input.known_targets,
        })
    }

    fn admitted_configured_target_dir(
        input: AdmittedConfiguredTargetDirInput<'_>,
    ) -> Option<PathBuf> {
        let configured = fs::canonicalize(input.configured_target_dir).ok()?;
        configured_target_matches_executable(ConfiguredTargetMatchInput {
            current_executable: input.current_executable,
            candidate: &configured,
            known_targets: input.known_targets,
        })
        .then_some(configured)
    }

    fn resolved_relative_configured_target_dir(
        input: RelativeConfiguredTargetDirInput<'_>,
    ) -> Option<PathBuf> {
        (!input.configured.is_absolute())
            .then(|| input.invocation_directory.join(input.configured))
            .and_then(|candidate| fs::canonicalize(candidate).ok())
            .filter(|candidate| {
                configured_target_matches_executable(ConfiguredTargetMatchInput {
                    current_executable: input.current_executable,
                    candidate,
                    known_targets: input.known_targets,
                })
            })
    }

    #[track_caller]
    fn resolved_or_executable_configured_target_dir(
        input: RelativeConfiguredTargetDirInput<'_>,
    ) -> PathBuf {
        let current_executable = input.current_executable;
        let configured = input.configured;
        let known_targets = input.known_targets;
        resolved_relative_configured_target_dir(input).unwrap_or_else(|| {
            configured_cargo_target_dir_for(ConfiguredCargoTargetDirInput {
                current_executable,
                configured,
                known_targets,
            })
        })
    }

    fn configured_target_matches_executable(input: ConfiguredTargetMatchInput<'_>) -> bool {
        let artifact_parent = input
            .current_executable
            .parent()
            .and_then(Path::parent)
            .and_then(Path::parent);
        artifact_parent.is_some_and(|artifact_parent| {
            artifact_parent == input.candidate
                || artifact_parent.parent() == Some(input.candidate)
                    && artifact_parent
                        .file_name()
                        .is_some_and(|name| input.known_targets.contains(name))
        })
    }

    #[track_caller]
    fn configured_cargo_target_dir_for(input: ConfiguredCargoTargetDirInput<'_>) -> PathBuf {
        let current = input.current_executable;
        let configured = input.configured;
        let known_targets = input.known_targets;
        if configured.is_absolute() {
            return configured.to_path_buf();
        }
        let configured = lexically_normalized(configured);
        let artifact_parent = current
            .parent()
            .and_then(Path::parent)
            .and_then(Path::parent)
            .expect("test executable has a Cargo artifact parent");
        if configured.as_os_str().is_empty() {
            assert!(
                artifact_parent
                    .file_name()
                    .is_none_or(|name| !known_targets.contains(name)),
                "dot-relative Cargo target directory is ambiguous without invocation provenance"
            );
            return artifact_parent.to_path_buf();
        }
        if configured.file_name().is_none() {
            return target_root_from_artifact_parent(artifact_parent, known_targets);
        }
        let Some(closest) = artifact_parent
            .ancestors()
            .find(|ancestor| ancestor.ends_with(&configured))
            .or_else(|| {
                let configured_name = configured.file_name()?;
                artifact_parent
                    .ancestors()
                    .find(|ancestor| ancestor.file_name() == Some(configured_name))
            })
        else {
            assert!(
                artifact_parent
                    .file_name()
                    .is_none_or(|name| !known_targets.contains(name)),
                "relative Cargo target directory is ambiguous when its root cannot be recovered"
            );
            return artifact_parent.to_path_buf();
        };
        if closest == artifact_parent
            && artifact_parent
                .file_name()
                .is_some_and(|name| known_targets.contains(name))
            && closest.parent().and_then(Path::file_name) == configured.file_name()
        {
            return closest
                .parent()
                .expect("repeated relative target name has an outer target root")
                .to_path_buf();
        }
        closest.to_path_buf()
    }

    #[track_caller]
    fn target_root_from_artifact_parent(
        artifact_parent: &Path,
        known_targets: &BTreeSet<OsString>,
    ) -> PathBuf {
        if artifact_parent
            .file_name()
            .is_some_and(|name| known_targets.contains(name))
        {
            return artifact_parent
                .parent()
                .expect("target-specific artifacts have a target directory")
                .to_path_buf();
        }
        artifact_parent.to_path_buf()
    }

    #[track_caller]
    fn canonicalized_target_dir(configured: &Path) -> PathBuf {
        fs::canonicalize(configured).expect("configured Cargo target directory canonicalizes")
    }

    #[track_caller]
    fn cargo_metadata_target_dir() -> PathBuf {
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
        let mut command = Command::new(cargo);
        command.args(["metadata", "--no-deps", "--format-version", "1"]);
        let Some(output) = bounded_command_output(&mut command, BRIDGE_DISCOVERY_TIMEOUT)
            .expect("Cargo target metadata is available")
        else {
            panic!("Cargo target metadata exceeded its timeout");
        };
        assert!(output.status.success(), "Cargo target metadata succeeds");
        let metadata: serde_json::Value =
            serde_json::from_slice(&output.stdout).expect("Cargo target metadata is valid JSON");
        let target_dir = metadata["target_directory"]
            .as_str()
            .expect("Cargo target metadata names the artifact directory");
        canonicalized_target_dir(Path::new(target_dir))
    }

    struct BridgeBuildTargetCandidates<'a> {
        configured: Option<&'a Path>,
        executable_artifact: Option<&'a Path>,
    }

    fn bridge_build_target_dir(
        candidates: BridgeBuildTargetCandidates<'_>,
        metadata_target_dir: impl FnOnce() -> PathBuf,
    ) -> PathBuf {
        candidates
            .configured
            .or(candidates.executable_artifact)
            .map(Path::to_path_buf)
            .unwrap_or_else(metadata_target_dir)
    }

    fn cargo_target_dir_from_artifact(current_executable: &Path) -> Option<PathBuf> {
        let artifact_parent = current_executable
            .parent()
            .and_then(Path::parent)
            .and_then(Path::parent)?;
        artifact_parent
            .ancestors()
            .take(2)
            .find(|candidate| {
                candidate
                    .join(CARGO_TARGET_DIRECTORY_MARKER_FILENAME)
                    .is_file()
            })
            .map(Path::to_path_buf)
    }

    #[track_caller]
    fn reject_unrecognized_default_target(input: DefaultTargetRecognition<'_>) {
        if input.configured_target_dir.is_some() {
            return;
        }
        let current = lexically_normalized(input.current_executable);
        let default_target_dir = lexically_normalized(input.default_target_dir);
        let artifact_parent = current
            .parent()
            .and_then(Path::parent)
            .and_then(Path::parent)
            .expect("test executable has a Cargo artifact parent");
        if artifact_parent == default_target_dir {
            return;
        }
        let artifact_target_dir = input
            .artifact_target_dir
            .map(lexically_normalized)
            .unwrap_or_else(|| default_target_dir.clone());
        if artifact_parent == artifact_target_dir {
            return;
        }
        let artifact_parent_name = artifact_parent
            .file_name()
            .expect("Cargo artifact parent has a name");
        assert!(
            artifact_parent.parent() == Some(artifact_target_dir.as_path())
                && input.known_targets.contains(artifact_parent_name),
            "custom Cargo target specifications are unsupported by the nested bridge build"
        );
    }

    #[track_caller]
    fn rustc_target_names(invocation_directory: &Path) -> BTreeSet<OsString> {
        let rustc = normalized_rustc_override(invocation_directory)
            .unwrap_or_else(|| PathBuf::from("rustc"));
        let mut command = Command::new(rustc);
        command.args(["--print", "target-list"]);
        let Some(output) = bounded_command_output(&mut command, BRIDGE_DISCOVERY_TIMEOUT)
            .expect("rustc target inventory is available")
        else {
            panic!("rustc target inventory exceeded its timeout");
        };
        assert!(output.status.success(), "rustc target inventory succeeds");
        String::from_utf8(output.stdout)
            .expect("rustc target inventory is UTF-8")
            .lines()
            .map(OsString::from)
            .collect()
    }

    fn normalized_rustc_override(invocation_directory: &Path) -> Option<PathBuf> {
        let configured = std::env::var_os("RUSTC")?;
        let configured_path = Path::new(&configured);
        assert!(
            configured_path.is_absolute()
                || is_bare_program_name(configured_path)
                || invocation_directory.is_absolute(),
            "relative RUSTC requires absolute parent Cargo invocation provenance"
        );
        Some(rustc_command_for(
            Some(&configured),
            Some(invocation_directory),
        ))
    }

    fn rustc_command_for(
        configured: Option<&OsStr>,
        invocation_directory: Option<&Path>,
    ) -> PathBuf {
        let Some(configured) = configured else {
            return PathBuf::from("rustc");
        };
        let configured = Path::new(configured);
        if configured.is_absolute() || is_bare_program_name(configured) {
            return configured.to_path_buf();
        }
        invocation_directory
            .map(|directory| directory.join(configured))
            .unwrap_or_else(|| PathBuf::from("rustc"))
    }

    /// Whether one configured command is a single program name.
    ///
    /// Cargo resolves a single-component override such as `rustc` or `sccache`
    /// through `PATH`; it is not a relative filesystem path, so joining it to
    /// the invocation directory names an executable that does not exist.
    fn is_bare_program_name(configured: &Path) -> bool {
        let mut components = configured.components();
        matches!(components.next(), Some(std::path::Component::Normal(_)))
            && components.next().is_none()
    }

    fn configure_compiler_wrapper(
        command: &mut Command,
        variable: &'static str,
        invocation_directory: &Path,
    ) {
        let Some(configured) = std::env::var_os(variable) else {
            return;
        };
        if configured.is_empty() {
            command.env_remove(variable);
            return;
        }
        let wrapper = compiler_wrapper_command_for(&configured, Some(invocation_directory))
            .unwrap_or_else(|| {
                panic!("relative {variable} requires parent Cargo invocation provenance")
            });
        command.env(variable, wrapper);
    }

    fn compiler_wrapper_command_for(
        configured: &OsStr,
        invocation_directory: Option<&Path>,
    ) -> Option<PathBuf> {
        let configured = Path::new(configured);
        if configured.as_os_str().is_empty() {
            return None;
        }
        if configured.is_absolute() || is_bare_program_name(configured) {
            return Some(configured.to_path_buf());
        }
        invocation_directory.map(|directory| directory.join(configured))
    }

    fn lexically_normalized(path: &Path) -> PathBuf {
        path.components()
            .fold(PathBuf::new(), |mut result, component| {
                match component {
                    std::path::Component::Prefix(prefix) => result.push(prefix.as_os_str()),
                    std::path::Component::RootDir => result.push(component.as_os_str()),
                    std::path::Component::CurDir => {}
                    std::path::Component::ParentDir => {
                        if !result.pop() {
                            result.push(component.as_os_str());
                        }
                    }
                    std::path::Component::Normal(part) => result.push(part),
                }
                result
            })
    }

    #[track_caller]
    fn claude_mcp_bridge_artifact_selection_for(
        input: BridgeArtifactSelectionInput<'_>,
    ) -> BridgeArtifactSelection {
        let current = lexically_normalized(input.current_executable);
        let configured_target_dir = input.configured_target_dir.map(lexically_normalized);
        let default_target_dir = lexically_normalized(input.default_target_dir);
        let profile_dir = current
            .parent()
            .and_then(Path::parent)
            .expect("test executable is under the Cargo profile directory");
        let profile_dir_name = profile_dir
            .file_name()
            .expect("Cargo profile directory has a name");
        let profile = match profile_dir_name.to_str() {
            Some(CARGO_DEBUG_PROFILE_DIRECTORY) | Some(CARGO_RELEASE_PROFILE) => {
                OsString::from(input.debug_profile)
            }
            _ => profile_dir_name.to_os_string(),
        };
        let artifact_parent = profile_dir
            .parent()
            .expect("Cargo profile has an artifact parent");
        let (target_dir, target) = if let Some(target_dir) = configured_target_dir.as_deref() {
            if artifact_parent == target_dir {
                (target_dir.to_path_buf(), None)
            } else {
                let artifact_parent_name = artifact_parent
                    .file_name()
                    .expect("target-specific Cargo artifact parent has a name");
                assert_eq!(
                    artifact_parent.parent(),
                    Some(target_dir),
                    "Cargo target-specific profile is directly below the configured target directory"
                );
                assert!(
                    input.known_targets.contains(artifact_parent_name),
                    "custom Cargo target specifications are unsupported by the nested bridge build"
                );
                (
                    target_dir.to_path_buf(),
                    Some(artifact_parent_name.to_os_string()),
                )
            }
        } else {
            let artifact_target_dir = input.artifact_target_dir.map(lexically_normalized);
            let recognized_target = artifact_parent.file_name().filter(|name| {
                artifact_parent != default_target_dir
                    && artifact_target_dir.as_deref() != Some(artifact_parent)
                    && input.known_targets.contains(*name)
            });
            recognized_target.map_or_else(
                || (artifact_parent.to_path_buf(), None),
                |target| {
                    (
                        artifact_parent
                            .parent()
                            .expect("Cargo target-specific artifacts have a target directory")
                            .to_path_buf(),
                        Some(target.to_os_string()),
                    )
                },
            )
        };
        BridgeArtifactSelection {
            profile,
            target,
            target_dir,
        }
    }

    struct BridgeArtifactExpectation<'a> {
        executable: &'a Path,
        target_dir: &'a Path,
        configured_target_dir: Option<&'a Path>,
        default_target_dir: &'a Path,
        debug_profile: &'a str,
        expected_profile: &'a str,
        expected_target: Option<&'a str>,
        recognized_target: Option<&'a str>,
    }

    #[track_caller]
    fn assert_bridge_artifact_selection(expectation: BridgeArtifactExpectation<'_>) {
        let known_targets = expectation
            .recognized_target
            .map(OsString::from)
            .into_iter()
            .collect();
        let selection = claude_mcp_bridge_artifact_selection_for(BridgeArtifactSelectionInput {
            current_executable: expectation.executable,
            debug_profile: expectation.debug_profile,
            configured_target_dir: expectation.configured_target_dir,
            default_target_dir: expectation.default_target_dir,
            artifact_target_dir: Some(expectation.target_dir),
            known_targets: &known_targets,
        });

        assert_eq!(
            selection.profile,
            OsString::from(expectation.expected_profile)
        );
        assert_eq!(
            selection.target,
            expectation.expected_target.map(OsString::from)
        );
        assert_eq!(selection.target_dir, expectation.target_dir);
    }

    #[test]
    fn bridge_artifact_selection_maps_debug_to_the_explicit_test_profile() {
        let target_dir = Path::new("synthetic-target");
        let executable = target_dir.join("debug/deps/daemon-tools-test");

        assert_bridge_artifact_selection(BridgeArtifactExpectation {
            executable: &executable,
            target_dir,
            configured_target_dir: None,
            default_target_dir: target_dir,
            debug_profile: CARGO_TEST_PROFILE,
            expected_profile: CARGO_TEST_PROFILE,
            expected_target: None,
            recognized_target: None,
        });
    }

    #[test]
    fn bridge_artifact_selection_preserves_an_explicit_dev_profile() {
        let arguments = [
            OsStr::new("cargo"),
            OsStr::new(CARGO_TEST_SUBCOMMAND),
            OsStr::new(CARGO_PROFILE_OPTION),
            OsStr::new(CARGO_DEV_PROFILE),
        ];
        let profile = cargo_test_profile_from_arguments(&arguments)
            .expect("the synthetic Cargo test invocation names a profile");
        let executable = Path::new("synthetic-target/debug/deps/daemon-tools-test");

        assert_bridge_artifact_selection(BridgeArtifactExpectation {
            executable,
            target_dir: Path::new("synthetic-target"),
            configured_target_dir: None,
            default_target_dir: Path::new("synthetic-target"),
            debug_profile: profile
                .to_str()
                .expect("the synthetic Cargo profile is valid UTF-8"),
            expected_profile: CARGO_DEV_PROFILE,
            expected_target: None,
            recognized_target: None,
        });
    }

    #[test]
    fn bridge_artifact_selection_preserves_the_release_profile() {
        let executable = Path::new("synthetic-target/release/deps/daemon-tools-test");

        assert_bridge_artifact_selection(BridgeArtifactExpectation {
            executable,
            target_dir: Path::new("synthetic-target"),
            configured_target_dir: None,
            default_target_dir: Path::new("synthetic-target"),
            debug_profile: CARGO_RELEASE_PROFILE,
            expected_profile: CARGO_RELEASE_PROFILE,
            expected_target: None,
            recognized_target: None,
        });
    }

    #[test]
    fn cargo_test_invocation_recognizes_the_short_release_option() {
        let arguments = [
            OsStr::new(CARGO_PROGRAM_STEM),
            OsStr::new(CARGO_TEST_SUBCOMMAND),
            OsStr::new(CARGO_RELEASE_SHORT_OPTION),
        ];

        assert_eq!(
            cargo_test_profile_from_arguments(&arguments),
            Some(OsString::from(CARGO_RELEASE_PROFILE))
        );
    }

    #[test]
    fn bridge_artifact_selection_preserves_an_explicit_bench_profile() {
        let arguments = [
            OsStr::new(CARGO_PROGRAM_STEM),
            OsStr::new(CARGO_TEST_SUBCOMMAND),
            OsStr::new(CARGO_PROFILE_OPTION),
            OsStr::new(CARGO_BENCH_PROFILE),
        ];
        let profile = cargo_test_profile_from_arguments(&arguments)
            .expect("the synthetic Cargo test invocation names a profile");
        let executable = Path::new("synthetic-target/release/deps/daemon-tools-test");

        assert_bridge_artifact_selection(BridgeArtifactExpectation {
            executable,
            target_dir: Path::new("synthetic-target"),
            configured_target_dir: None,
            default_target_dir: Path::new("synthetic-target"),
            debug_profile: profile
                .to_str()
                .expect("the synthetic Cargo profile is valid UTF-8"),
            expected_profile: CARGO_BENCH_PROFILE,
            expected_target: None,
            recognized_target: None,
        });
    }

    #[test]
    fn cargo_test_profile_accepts_the_builtin_test_alias() {
        let arguments = [
            OsStr::new(CARGO_PROGRAM_STEM),
            OsStr::new(CARGO_TEST_SUBCOMMAND_ALIAS),
            OsStr::new(CARGO_PROFILE_OPTION),
            OsStr::new(CARGO_DEV_PROFILE),
        ];

        assert_eq!(
            cargo_test_profile_from_arguments(&arguments),
            Some(OsString::from(CARGO_DEV_PROFILE))
        );
    }

    #[test]
    fn cargo_test_profile_consumes_values_of_global_options() {
        let arguments = [
            OsStr::new(CARGO_PROGRAM_STEM),
            OsStr::new(CARGO_COLOR_OPTION),
            OsStr::new(SYNTHETIC_CARGO_COLOR_OPTION_VALUE),
            OsStr::new(CARGO_CHANGE_DIRECTORY_OPTION),
            OsStr::new(SYNTHETIC_CARGO_CHANGE_DIRECTORY_OPTION_VALUE),
            OsStr::new(CARGO_UNSTABLE_OPTION),
            OsStr::new(SYNTHETIC_CARGO_UNSTABLE_OPTION_VALUE),
            OsStr::new(CARGO_TEST_SUBCOMMAND),
            OsStr::new(CARGO_PROFILE_OPTION),
            OsStr::new(CARGO_DEV_PROFILE),
        ];

        assert_eq!(
            cargo_test_profile_from_arguments(&arguments),
            Some(OsString::from(CARGO_DEV_PROFILE))
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn cargo_invocation_directory_reads_the_parent_process_working_directory() {
        let process_directory = tempfile::tempdir().expect("synthetic process directory exists");
        let invocation_directory =
            tempfile::tempdir().expect("synthetic Cargo invocation directory exists");
        std::os::unix::fs::symlink(
            invocation_directory.path(),
            process_directory
                .path()
                .join(PROC_WORKING_DIRECTORY_FILENAME),
        )
        .expect("synthetic process working-directory link is created");

        assert_eq!(
            cargo_invocation_directory(process_directory.path())
                .expect("synthetic process working-directory link resolves"),
            invocation_directory.path()
        );
    }

    #[test]
    fn cargo_test_invocation_retains_the_parent_working_directory() {
        let invocation_directory = Path::new("/synthetic/parent-cargo-cwd");
        let arguments = [
            OsStr::new(CARGO_PROGRAM_STEM),
            OsStr::new(CARGO_TEST_SUBCOMMAND),
        ];
        let invocation = cargo_test_invocation_from_arguments(&arguments, invocation_directory)
            .expect("the synthetic Cargo test invocation is admitted");

        assert_eq!(invocation.invocation_directory, invocation_directory);
    }

    #[cfg(unix)]
    #[test]
    fn relative_target_dir_uses_the_captured_cargo_change_directory() {
        let invocation_directory = tempfile::tempdir().expect("synthetic Cargo cwd exists");
        let target_directory = invocation_directory.path().join("resolved-target");
        let configured_target = Path::new("target-link");
        fs::create_dir(&target_directory).expect("synthetic target directory exists");
        std::os::unix::fs::symlink(
            &target_directory,
            invocation_directory.path().join(configured_target),
        )
        .expect("synthetic relative target link exists");
        let executable = target_directory.join("debug/deps/daemon-tools-test");
        let arguments = [
            OsStr::new(CARGO_PROGRAM_STEM),
            OsStr::new(CARGO_UNSTABLE_OPTION),
            OsStr::new(SYNTHETIC_CARGO_UNSTABLE_OPTION_VALUE),
            OsStr::new(CARGO_CHANGE_DIRECTORY_OPTION),
            invocation_directory.path().as_os_str(),
            OsStr::new(CARGO_TEST_SUBCOMMAND),
        ];
        let invocation =
            cargo_test_invocation_from_arguments(&arguments, invocation_directory.path())
                .expect("the changed-directory Cargo test invocation is admitted");

        assert_eq!(
            resolved_relative_configured_target_dir(RelativeConfiguredTargetDirInput {
                current_executable: &executable,
                configured: configured_target,
                invocation_directory: &invocation.invocation_directory,
                known_targets: &BTreeSet::new(),
            }),
            Some(target_directory)
        );
    }

    #[test]
    fn cargo_test_invocation_rejects_an_ambiguous_direct_debug_artifact() {
        let workspace = Path::new("/synthetic/workspace");
        let executable = Path::new("/synthetic/target/debug/deps/daemon-tools-test");
        let invocation = cargo_test_invocation_from_artifact(CargoTestArtifactInvocation {
            current_executable: executable,
            invocation_directory: workspace,
        });

        assert_eq!(invocation, None);
    }

    #[test]
    fn cargo_test_invocation_rejects_an_ambiguous_direct_release_artifact() {
        let workspace = Path::new("/synthetic/workspace");
        let executable = Path::new("/synthetic/target/release/deps/daemon-tools-test");
        let invocation = cargo_test_invocation_from_artifact(CargoTestArtifactInvocation {
            current_executable: executable,
            invocation_directory: workspace,
        });

        assert_eq!(invocation, None);
    }

    #[test]
    fn cargo_test_invocation_preserves_an_unambiguous_direct_custom_profile() {
        let workspace = Path::new("/synthetic/workspace");
        let executable = Path::new("/synthetic/target/ci-fast/deps/daemon-tools-test");
        let invocation = cargo_test_invocation_from_artifact(CargoTestArtifactInvocation {
            current_executable: executable,
            invocation_directory: workspace,
        })
        .expect("the custom profile directory is unambiguous");

        assert_eq!(invocation.profile, OsStr::new("ci-fast"));
        assert_eq!(invocation.invocation_directory, workspace);
        assert!(invocation.config_overrides.is_empty());
        assert!(invocation.unstable_flags.is_empty());
        assert!(!invocation.ignore_rust_version);
    }

    #[test]
    fn cargo_test_profile_rejects_an_unexpanded_configured_alias() {
        let arguments = [
            OsStr::new(CARGO_PROGRAM_STEM),
            OsStr::new("configured-test-alias"),
        ];

        assert_eq!(cargo_test_profile_from_arguments(&arguments), None);
    }

    #[test]
    fn bridge_build_preserves_parent_cargo_config_overrides() {
        let invocation_directory = Path::new("/synthetic/invocation");
        let key_value = OsStr::new("profile.test.overflow-checks=false");
        let relative_path = OsStr::new("config/bridge.toml");
        let arguments = [
            OsStr::new(CARGO_PROGRAM_STEM),
            OsStr::new(CARGO_CONFIG_OPTION),
            key_value,
            OsStr::new(CARGO_TEST_SUBCOMMAND),
            OsStr::new(CARGO_CONFIG_OPTION),
            relative_path,
            OsStr::new(CARGO_PROFILE_OPTION),
            OsStr::new(CARGO_DEV_PROFILE),
        ];
        let invocation = cargo_test_invocation_from_arguments(&arguments, invocation_directory)
            .expect("the synthetic Cargo test invocation is admitted");
        let expected_path = invocation_directory.join(relative_path);
        let mut command = Command::new(CARGO_PROGRAM_STEM);
        apply_cargo_config_overrides(&mut command, &invocation.config_overrides);

        assert_eq!(invocation.profile, OsString::from(CARGO_DEV_PROFILE));
        assert_eq!(
            invocation.config_overrides,
            vec![
                key_value.to_os_string(),
                expected_path.clone().into_os_string()
            ]
        );
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            vec![
                OsStr::new(CARGO_CONFIG_OPTION),
                key_value,
                OsStr::new(CARGO_CONFIG_OPTION),
                expected_path.as_os_str(),
            ]
        );
    }

    #[test]
    fn bridge_build_preserves_the_parent_rust_version_override() {
        let arguments = [
            OsStr::new(CARGO_PROGRAM_STEM),
            OsStr::new(CARGO_TEST_SUBCOMMAND),
            OsStr::new(CARGO_IGNORE_RUST_VERSION_OPTION),
        ];
        let invocation = cargo_test_invocation_from_arguments(&arguments, Path::new("."))
            .expect("the synthetic Cargo test invocation is admitted");
        let mut command = Command::new(CARGO_PROGRAM_STEM);
        apply_cargo_rust_version_policy(&mut command, invocation.ignore_rust_version);

        assert!(invocation.ignore_rust_version);
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            vec![OsStr::new(CARGO_IGNORE_RUST_VERSION_OPTION)]
        );
    }

    #[test]
    fn bridge_build_preserves_parent_cargo_config_hierarchy() {
        let invocation_directory = Path::new("/synthetic/invocation");
        let workspace = Path::new("/synthetic/workspace");
        let expected_manifest = workspace.join(CARGO_MANIFEST_FILENAME);
        let mut command = Command::new(CARGO_PROGRAM_STEM);
        configure_bridge_build_location(
            &mut command,
            BridgeBuildLocation {
                invocation_directory,
                workspace,
            },
        );

        assert_eq!(command.get_current_dir(), Some(invocation_directory));
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            vec![
                OsStr::new(CARGO_MANIFEST_PATH_OPTION),
                expected_manifest.as_os_str(),
            ]
        );
    }

    #[test]
    fn bridge_build_preserves_parent_cargo_unstable_flags() {
        let attached_unstable_option =
            format!("{CARGO_UNSTABLE_OPTION}{SYNTHETIC_CARGO_UNSTABLE_OPTION_VALUE}");
        let arguments = [
            OsStr::new(CARGO_PROGRAM_STEM),
            OsStr::new(&attached_unstable_option),
            OsStr::new(CARGO_TEST_SUBCOMMAND),
            OsStr::new(CARGO_UNSTABLE_OPTION),
            OsStr::new(SYNTHETIC_POST_SUBCOMMAND_UNSTABLE_OPTION_VALUE),
        ];
        let invocation = cargo_test_invocation_from_arguments(&arguments, Path::new("."))
            .expect("the synthetic Cargo test invocation is admitted");
        let mut command = Command::new(CARGO_PROGRAM_STEM);
        apply_cargo_unstable_flags(&mut command, &invocation.unstable_flags);

        assert_eq!(
            invocation.unstable_flags,
            vec![
                OsString::from(SYNTHETIC_CARGO_UNSTABLE_OPTION_VALUE),
                OsString::from(SYNTHETIC_POST_SUBCOMMAND_UNSTABLE_OPTION_VALUE),
            ]
        );
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            vec![
                OsStr::new(CARGO_UNSTABLE_OPTION),
                OsStr::new(SYNTHETIC_CARGO_UNSTABLE_OPTION_VALUE),
                OsStr::new(CARGO_UNSTABLE_OPTION),
                OsStr::new(SYNTHETIC_POST_SUBCOMMAND_UNSTABLE_OPTION_VALUE),
            ]
        );
    }

    #[test]
    fn bridge_artifact_selection_preserves_a_custom_profile() {
        let custom_profile = "ci-fast";
        let executable = Path::new("synthetic-target")
            .join(custom_profile)
            .join("deps/daemon-tools-test");

        assert_bridge_artifact_selection(BridgeArtifactExpectation {
            executable: &executable,
            target_dir: Path::new("synthetic-target"),
            configured_target_dir: None,
            default_target_dir: Path::new("synthetic-target"),
            debug_profile: CARGO_TEST_PROFILE,
            expected_profile: custom_profile,
            expected_target: None,
            recognized_target: None,
        });
    }

    const SYNTHETIC_CARGO_TARGET: &str = "x86_64-unknown-linux-musl";

    fn synthetic_known_targets() -> BTreeSet<OsString> {
        [OsString::from(SYNTHETIC_CARGO_TARGET)]
            .into_iter()
            .collect()
    }

    #[test]
    fn bridge_artifact_selection_preserves_a_cli_selected_target() {
        let executable = Path::new("synthetic-target")
            .join(SYNTHETIC_CARGO_TARGET)
            .join("debug/deps/daemon-tools-test");

        assert_bridge_artifact_selection(BridgeArtifactExpectation {
            executable: &executable,
            target_dir: Path::new("synthetic-target"),
            configured_target_dir: None,
            default_target_dir: Path::new("synthetic-target"),
            debug_profile: CARGO_TEST_PROFILE,
            expected_profile: CARGO_TEST_PROFILE,
            expected_target: Some(SYNTHETIC_CARGO_TARGET),
            recognized_target: Some(SYNTHETIC_CARGO_TARGET),
        });
    }

    #[test]
    fn bridge_artifact_selection_preserves_a_target_with_a_cli_target_directory() {
        let target_dir = Path::new("synthetic-parent");
        let executable = target_dir
            .join(SYNTHETIC_CARGO_TARGET)
            .join("debug/deps/daemon-tools-test");

        assert_bridge_artifact_selection(BridgeArtifactExpectation {
            executable: &executable,
            target_dir,
            configured_target_dir: None,
            default_target_dir: Path::new("synthetic-default-target"),
            debug_profile: CARGO_TEST_PROFILE,
            expected_profile: CARGO_TEST_PROFILE,
            expected_target: Some(SYNTHETIC_CARGO_TARGET),
            recognized_target: Some(SYNTHETIC_CARGO_TARGET),
        });
    }

    #[test]
    #[should_panic(
        expected = "custom Cargo target specifications are unsupported by the nested bridge build"
    )]
    fn bridge_artifact_selection_rejects_a_custom_target_specification() {
        let custom_target = "synthetic-custom-target";
        let target_dir = Path::new("synthetic-target");
        let executable = target_dir
            .join(custom_target)
            .join("debug/deps/daemon-tools-test");

        assert_bridge_artifact_selection(BridgeArtifactExpectation {
            executable: &executable,
            target_dir,
            configured_target_dir: Some(target_dir),
            default_target_dir: target_dir,
            debug_profile: CARGO_TEST_PROFILE,
            expected_profile: CARGO_TEST_PROFILE,
            expected_target: None,
            recognized_target: None,
        });
    }

    #[test]
    #[should_panic(
        expected = "custom Cargo target specifications are unsupported by the nested bridge build"
    )]
    fn bridge_artifact_selection_rejects_an_unrecognized_default_target() {
        let target_dir = Path::new("synthetic-target");
        let executable = target_dir.join("custom/debug/deps/daemon-tools-test");

        reject_unrecognized_default_target(DefaultTargetRecognition {
            current_executable: &executable,
            configured_target_dir: None,
            default_target_dir: target_dir,
            artifact_target_dir: Some(target_dir),
            known_targets: &BTreeSet::new(),
        });
    }

    #[test]
    fn bridge_artifact_selection_accepts_a_host_build_with_a_cli_target_directory() {
        let cli_target_dir = Path::new("synthetic-cli-target");
        let executable = cli_target_dir.join("debug/deps/daemon-tools-test");

        reject_unrecognized_default_target(DefaultTargetRecognition {
            current_executable: &executable,
            configured_target_dir: None,
            default_target_dir: Path::new("synthetic-default-target"),
            artifact_target_dir: Some(cli_target_dir),
            known_targets: &BTreeSet::new(),
        });
        assert_bridge_artifact_selection(BridgeArtifactExpectation {
            executable: &executable,
            target_dir: cli_target_dir,
            configured_target_dir: None,
            default_target_dir: Path::new("synthetic-default-target"),
            debug_profile: CARGO_TEST_PROFILE,
            expected_profile: CARGO_TEST_PROFILE,
            expected_target: None,
            recognized_target: None,
        });
    }

    #[test]
    fn bridge_artifact_selection_prefers_a_cli_root_over_stale_inherited_configuration() {
        let fixture = tempfile::tempdir().expect("fixture root exists");
        let cli_target_dir = fixture.path().join("synthetic-cli-target");
        let stale_target_dir = fixture.path().join("synthetic-stale-target");
        fs::create_dir(&cli_target_dir).expect("CLI target directory exists");
        fs::create_dir(&stale_target_dir).expect("stale target directory exists");
        let executable = cli_target_dir.join("debug/deps/daemon-tools-test");
        let configured = admitted_configured_target_dir(AdmittedConfiguredTargetDirInput {
            current_executable: &executable,
            configured_target_dir: &stale_target_dir,
            known_targets: &BTreeSet::new(),
        });

        assert_eq!(configured, None);
        assert_bridge_artifact_selection(BridgeArtifactExpectation {
            executable: &executable,
            target_dir: &cli_target_dir,
            configured_target_dir: configured.as_deref(),
            default_target_dir: Path::new("synthetic-default-target"),
            debug_profile: CARGO_TEST_PROFILE,
            expected_profile: CARGO_TEST_PROFILE,
            expected_target: None,
            recognized_target: None,
        });
    }

    #[test]
    fn bridge_artifact_selection_ignores_a_nonexistent_stale_target_directory() {
        let fixture = tempfile::tempdir().expect("fixture root exists");
        let cli_target_dir = fixture.path().join("synthetic-cli-target");
        let stale_target_dir = fixture.path().join("synthetic-stale-target");
        fs::create_dir(&cli_target_dir).expect("CLI target directory exists");
        let executable = cli_target_dir.join("debug/deps/daemon-tools-test");
        let configured = admitted_configured_target_dir(AdmittedConfiguredTargetDirInput {
            current_executable: &executable,
            configured_target_dir: &stale_target_dir,
            known_targets: &BTreeSet::new(),
        });

        assert_eq!(configured, None);
        assert_bridge_artifact_selection(BridgeArtifactExpectation {
            executable: &executable,
            target_dir: &cli_target_dir,
            configured_target_dir: configured.as_deref(),
            default_target_dir: Path::new("synthetic-default-target"),
            debug_profile: CARGO_TEST_PROFILE,
            expected_profile: CARGO_TEST_PROFILE,
            expected_target: None,
            recognized_target: None,
        });
    }

    #[test]
    fn bridge_build_prefers_the_executable_target_root_over_metadata() {
        let cli_target_dir = Path::new("synthetic-cli-target");

        let selected = bridge_build_target_dir(
            BridgeBuildTargetCandidates {
                configured: None,
                executable_artifact: Some(cli_target_dir),
            },
            || panic!("Cargo metadata must not override the executable target root"),
        );

        assert_eq!(selected, cli_target_dir);
    }

    #[test]
    fn bridge_artifact_selection_keeps_a_recognized_name_as_a_cli_host_root() {
        let cli_target_dir = Path::new(SYNTHETIC_CARGO_TARGET);
        let executable = cli_target_dir.join("debug/deps/daemon-tools-test");

        assert_bridge_artifact_selection(BridgeArtifactExpectation {
            executable: &executable,
            target_dir: cli_target_dir,
            configured_target_dir: None,
            default_target_dir: Path::new("synthetic-default-target"),
            debug_profile: CARGO_TEST_PROFILE,
            expected_profile: CARGO_TEST_PROFILE,
            expected_target: None,
            recognized_target: Some(SYNTHETIC_CARGO_TARGET),
        });
    }

    #[test]
    #[should_panic(
        expected = "custom Cargo target specifications are unsupported by the nested bridge build"
    )]
    fn bridge_artifact_selection_rejects_a_custom_target_with_a_cli_target_directory() {
        let cli_target_dir = Path::new("synthetic-cli-target");
        let executable = cli_target_dir.join("custom/debug/deps/daemon-tools-test");

        reject_unrecognized_default_target(DefaultTargetRecognition {
            current_executable: &executable,
            configured_target_dir: None,
            default_target_dir: Path::new("synthetic-default-target"),
            artifact_target_dir: Some(cli_target_dir),
            known_targets: &BTreeSet::new(),
        });
    }

    #[test]
    fn bridge_artifact_selection_discovers_a_cli_target_directory_from_its_marker() {
        let cli_target_dir = tempfile::tempdir().expect("CLI target directory is created");
        let executable = cli_target_dir
            .path()
            .join("custom/debug/deps/daemon-tools-test");
        fs::write(
            cli_target_dir
                .path()
                .join(CARGO_TARGET_DIRECTORY_MARKER_FILENAME),
            EMPTY_CARGO_TARGET_DIRECTORY_MARKER,
        )
        .expect("Cargo target directory marker is written");

        assert_eq!(
            cargo_target_dir_from_artifact(&executable),
            Some(cli_target_dir.path().to_path_buf())
        );
    }

    #[test]
    fn bridge_artifact_selection_normalizes_the_configured_target_directory() {
        let configured_target_dir = Path::new("synthetic-parent/../synthetic-target");
        let normalized_target_dir = Path::new("synthetic-target");
        let executable = normalized_target_dir.join("debug/deps/daemon-tools-test");

        assert_bridge_artifact_selection(BridgeArtifactExpectation {
            executable: &executable,
            target_dir: normalized_target_dir,
            configured_target_dir: Some(configured_target_dir),
            default_target_dir: normalized_target_dir,
            debug_profile: CARGO_TEST_PROFILE,
            expected_profile: CARGO_TEST_PROFILE,
            expected_target: None,
            recognized_target: None,
        });
    }

    #[cfg(unix)]
    #[test]
    fn bridge_artifact_selection_canonicalizes_a_symlinked_target_directory() {
        let parent = tempfile::tempdir().expect("fixture parent exists");
        let target_dir = parent.path().join("target-output");
        let target_link = parent.path().join("target-link");
        fs::create_dir(&target_dir).expect("fixture target directory exists");
        std::os::unix::fs::symlink(&target_dir, &target_link)
            .expect("fixture target symlink exists");

        assert_eq!(canonicalized_target_dir(&target_link), target_dir);
    }

    #[cfg(unix)]
    #[test]
    fn bridge_artifact_selection_recovers_a_differently_named_relative_symlink_root() {
        let parent = tempfile::tempdir().expect("fixture parent exists");
        let target_dir = parent.path().join("resolved-target");
        let target_link = parent.path().join("target-link");
        fs::create_dir(&target_dir).expect("fixture target directory exists");
        std::os::unix::fs::symlink(&target_dir, &target_link)
            .expect("fixture target symlink exists");
        let executable = target_dir.join("debug/deps/daemon-tools-test");
        let configured_target_dir =
            configured_cargo_target_dir_for(ConfiguredCargoTargetDirInput {
                current_executable: &executable,
                configured: Path::new("target-link"),
                known_targets: &BTreeSet::new(),
            });

        assert_bridge_artifact_selection(BridgeArtifactExpectation {
            executable: &executable,
            target_dir: &target_dir,
            configured_target_dir: Some(&configured_target_dir),
            default_target_dir: Path::new("synthetic-default-target"),
            debug_profile: CARGO_TEST_PROFILE,
            expected_profile: CARGO_TEST_PROFILE,
            expected_target: None,
            recognized_target: None,
        });
    }

    #[cfg(unix)]
    #[test]
    fn bridge_artifact_selection_resolves_a_relative_symlink_root_before_preserving_a_target() {
        let invocation = tempfile::tempdir().expect("fixture invocation root exists");
        let target_dir = invocation.path().join("resolved-target");
        let target_link = invocation.path().join("target-link");
        fs::create_dir(&target_dir).expect("fixture target directory exists");
        std::os::unix::fs::symlink(&target_dir, &target_link)
            .expect("fixture target symlink exists");
        let executable = target_dir
            .join(SYNTHETIC_CARGO_TARGET)
            .join("debug/deps/daemon-tools-test");
        let resolved = resolved_relative_configured_target_dir(RelativeConfiguredTargetDirInput {
            current_executable: &executable,
            configured: Path::new("target-link"),
            invocation_directory: invocation.path(),
            known_targets: &synthetic_known_targets(),
        })
        .expect("relative target symlink resolves from the invocation root");
        let configured_target_dir =
            configured_cargo_target_dir_for(ConfiguredCargoTargetDirInput {
                current_executable: &executable,
                configured: &resolved,
                known_targets: &synthetic_known_targets(),
            });

        assert_bridge_artifact_selection(BridgeArtifactExpectation {
            executable: &executable,
            target_dir: &target_dir,
            configured_target_dir: Some(&configured_target_dir),
            default_target_dir: Path::new("synthetic-default-target"),
            debug_profile: CARGO_TEST_PROFILE,
            expected_profile: CARGO_TEST_PROFILE,
            expected_target: Some(SYNTHETIC_CARGO_TARGET),
            recognized_target: Some(SYNTHETIC_CARGO_TARGET),
        });
    }

    #[test]
    #[should_panic(
        expected = "relative Cargo target directory is ambiguous when its root cannot be recovered"
    )]
    fn bridge_artifact_selection_rejects_a_recognized_name_as_an_unresolved_host_root() {
        let target_dir = Path::new("synthetic-parent").join(SYNTHETIC_CARGO_TARGET);
        let executable = target_dir.join("debug/deps/daemon-tools-test");
        configured_cargo_target_dir_for(ConfiguredCargoTargetDirInput {
            current_executable: &executable,
            configured: Path::new("target-link"),
            known_targets: &synthetic_known_targets(),
        });
    }

    #[cfg(unix)]
    #[test]
    #[should_panic(
        expected = "relative Cargo target directory is ambiguous when its root cannot be recovered"
    )]
    fn bridge_artifact_selection_rejects_a_hidden_symlink_root_for_a_target_build() {
        let parent = tempfile::tempdir().expect("fixture parent exists");
        let target_dir = parent.path().join("resolved-target");
        let target_link = parent.path().join("target-link");
        fs::create_dir(&target_dir).expect("fixture target directory exists");
        std::os::unix::fs::symlink(&target_dir, &target_link)
            .expect("fixture target symlink exists");
        let executable = target_dir
            .join(SYNTHETIC_CARGO_TARGET)
            .join("debug/deps/daemon-tools-test");

        configured_cargo_target_dir_for(ConfiguredCargoTargetDirInput {
            current_executable: &executable,
            configured: Path::new("target-link"),
            known_targets: &synthetic_known_targets(),
        });
    }

    #[test]
    fn bridge_artifact_selection_derives_a_relative_target_directory_from_the_executable() {
        let invocation = Path::new("synthetic-invocation");
        let target_dir = invocation.join("relative-target");
        let executable = target_dir.join("debug/deps/daemon-tools-test");

        assert_bridge_artifact_selection(BridgeArtifactExpectation {
            executable: &executable,
            target_dir: &target_dir,
            configured_target_dir: None,
            default_target_dir: &target_dir,
            debug_profile: CARGO_TEST_PROFILE,
            expected_profile: CARGO_TEST_PROFILE,
            expected_target: None,
            recognized_target: None,
        });
    }

    #[test]
    fn bridge_artifact_selection_preserves_a_target_with_a_relative_configured_directory() {
        let invocation = Path::new("synthetic-invocation");
        let target_dir = invocation.join("relative-target");
        let executable = target_dir
            .join(SYNTHETIC_CARGO_TARGET)
            .join("debug/deps/daemon-tools-test");
        let configured_target_dir =
            configured_cargo_target_dir_for(ConfiguredCargoTargetDirInput {
                current_executable: &executable,
                configured: Path::new("relative-target"),
                known_targets: &synthetic_known_targets(),
            });

        assert_bridge_artifact_selection(BridgeArtifactExpectation {
            executable: &executable,
            target_dir: &target_dir,
            configured_target_dir: Some(&configured_target_dir),
            default_target_dir: Path::new("synthetic-default-target"),
            debug_profile: CARGO_TEST_PROFILE,
            expected_profile: CARGO_TEST_PROFILE,
            expected_target: Some(SYNTHETIC_CARGO_TARGET),
            recognized_target: Some(SYNTHETIC_CARGO_TARGET),
        });
    }

    #[test]
    fn bridge_artifact_selection_excludes_the_profile_from_relative_root_matching() {
        let target_dir = Path::new("synthetic-workspace/debug");
        let executable = target_dir.join("debug/deps/daemon-tools-test");
        let configured_target_dir =
            configured_cargo_target_dir_for(ConfiguredCargoTargetDirInput {
                current_executable: &executable,
                configured: Path::new("debug"),
                known_targets: &BTreeSet::new(),
            });

        assert_bridge_artifact_selection(BridgeArtifactExpectation {
            executable: &executable,
            target_dir,
            configured_target_dir: Some(&configured_target_dir),
            default_target_dir: Path::new("synthetic-default-target"),
            debug_profile: CARGO_TEST_PROFILE,
            expected_profile: CARGO_TEST_PROFILE,
            expected_target: None,
            recognized_target: None,
        });
    }

    #[test]
    fn bridge_artifact_selection_resolves_a_parent_relative_configured_directory() {
        let target_dir = Path::new("synthetic-parent/artifact");
        let executable = target_dir
            .join(SYNTHETIC_CARGO_TARGET)
            .join("debug/deps/daemon-tools-test");
        let configured_target_dir =
            configured_cargo_target_dir_for(ConfiguredCargoTargetDirInput {
                current_executable: &executable,
                configured: Path::new("../artifact"),
                known_targets: &synthetic_known_targets(),
            });

        assert_bridge_artifact_selection(BridgeArtifactExpectation {
            executable: &executable,
            target_dir,
            configured_target_dir: Some(&configured_target_dir),
            default_target_dir: Path::new("synthetic-default-target"),
            debug_profile: CARGO_TEST_PROFILE,
            expected_profile: CARGO_TEST_PROFILE,
            expected_target: Some(SYNTHETIC_CARGO_TARGET),
            recognized_target: Some(SYNTHETIC_CARGO_TARGET),
        });
    }

    #[test]
    fn bridge_artifact_selection_preserves_an_identically_named_relative_root_and_target() {
        let target_dir = Path::new("synthetic-parent").join(SYNTHETIC_CARGO_TARGET);
        let executable = target_dir
            .join(SYNTHETIC_CARGO_TARGET)
            .join("debug/deps/daemon-tools-test");
        let configured = Path::new("..").join(SYNTHETIC_CARGO_TARGET);
        let configured_target_dir =
            configured_cargo_target_dir_for(ConfiguredCargoTargetDirInput {
                current_executable: &executable,
                configured: &configured,
                known_targets: &synthetic_known_targets(),
            });

        assert_bridge_artifact_selection(BridgeArtifactExpectation {
            executable: &executable,
            target_dir: &target_dir,
            configured_target_dir: Some(&configured_target_dir),
            default_target_dir: Path::new("synthetic-default-target"),
            debug_profile: CARGO_TEST_PROFILE,
            expected_profile: CARGO_TEST_PROFILE,
            expected_target: Some(SYNTHETIC_CARGO_TARGET),
            recognized_target: Some(SYNTHETIC_CARGO_TARGET),
        });
    }

    #[test]
    fn bridge_artifact_selection_keeps_a_repeated_unrecognized_name_as_the_host_root() {
        let target_dir = Path::new("synthetic-parent/artifact/artifact");
        let executable = target_dir.join("debug/deps/daemon-tools-test");
        let configured_target_dir =
            configured_cargo_target_dir_for(ConfiguredCargoTargetDirInput {
                current_executable: &executable,
                configured: Path::new("../artifact"),
                known_targets: &synthetic_known_targets(),
            });

        assert_bridge_artifact_selection(BridgeArtifactExpectation {
            executable: &executable,
            target_dir,
            configured_target_dir: Some(&configured_target_dir),
            default_target_dir: Path::new("synthetic-default-target"),
            debug_profile: CARGO_TEST_PROFILE,
            expected_profile: CARGO_TEST_PROFILE,
            expected_target: None,
            recognized_target: Some(SYNTHETIC_CARGO_TARGET),
        });
    }

    #[test]
    fn bridge_artifact_selection_resolves_a_dot_relative_configured_directory() {
        let target_dir = Path::new("synthetic-workspace");
        let executable = target_dir.join("debug/deps/daemon-tools-test");
        let configured_target_dir =
            configured_cargo_target_dir_for(ConfiguredCargoTargetDirInput {
                current_executable: &executable,
                configured: Path::new("."),
                known_targets: &BTreeSet::new(),
            });

        assert_bridge_artifact_selection(BridgeArtifactExpectation {
            executable: &executable,
            target_dir,
            configured_target_dir: Some(&configured_target_dir),
            default_target_dir: Path::new("synthetic-default-target"),
            debug_profile: CARGO_TEST_PROFILE,
            expected_profile: CARGO_TEST_PROFILE,
            expected_target: None,
            recognized_target: None,
        });
    }

    #[test]
    fn bridge_artifact_selection_keeps_a_recognized_name_as_a_dot_relative_host_root() {
        let parent = tempfile::tempdir().expect("fixture parent exists");
        let target_dir = parent.path().join(SYNTHETIC_CARGO_TARGET);
        fs::create_dir(&target_dir).expect("fixture target directory exists");
        let executable = target_dir.join("debug/deps/daemon-tools-test");
        let resolved = resolved_relative_configured_target_dir(RelativeConfiguredTargetDirInput {
            current_executable: &executable,
            configured: Path::new("."),
            invocation_directory: &target_dir,
            known_targets: &synthetic_known_targets(),
        })
        .expect("dot-relative target root resolves from the invocation directory");
        let configured_target_dir =
            configured_cargo_target_dir_for(ConfiguredCargoTargetDirInput {
                current_executable: &executable,
                configured: &resolved,
                known_targets: &synthetic_known_targets(),
            });

        assert_bridge_artifact_selection(BridgeArtifactExpectation {
            executable: &executable,
            target_dir: &target_dir,
            configured_target_dir: Some(&configured_target_dir),
            default_target_dir: Path::new("synthetic-default-target"),
            debug_profile: CARGO_TEST_PROFILE,
            expected_profile: CARGO_TEST_PROFILE,
            expected_target: None,
            recognized_target: Some(SYNTHETIC_CARGO_TARGET),
        });
    }

    #[test]
    fn bridge_artifact_selection_rejects_a_stale_pwd_target_and_recovers_from_the_executable() {
        let parent = tempfile::tempdir().expect("fixture parent exists");
        let actual_invocation = parent.path().join("actual-invocation");
        let stale_invocation = parent.path().join("stale-invocation");
        let configured = Path::new("target");
        let actual_target = actual_invocation.join(configured);
        let stale_target = stale_invocation.join(configured);
        fs::create_dir_all(&actual_target).expect("actual target directory exists");
        fs::create_dir_all(&stale_target).expect("stale target directory exists");
        let executable = actual_target.join("debug/deps/daemon-tools-test");

        let target_dir =
            resolved_or_executable_configured_target_dir(RelativeConfiguredTargetDirInput {
                current_executable: &executable,
                configured,
                invocation_directory: &stale_invocation,
                known_targets: &BTreeSet::new(),
            });

        assert_eq!(target_dir, actual_target);
    }

    #[test]
    #[should_panic(
        expected = "dot-relative Cargo target directory is ambiguous without invocation provenance"
    )]
    fn bridge_artifact_selection_rejects_an_ambiguous_dot_target_layout() {
        let target_dir = Path::new("synthetic-workspace");
        let artifact_root = target_dir.join(SYNTHETIC_CARGO_TARGET);
        let executable = artifact_root.join("debug/deps/daemon-tools-test");
        configured_cargo_target_dir_for(ConfiguredCargoTargetDirInput {
            current_executable: &executable,
            configured: Path::new("."),
            known_targets: &synthetic_known_targets(),
        });
    }

    #[test]
    fn bridge_artifact_selection_accepts_a_filesystem_root_target_directory() {
        let executable = Path::new("/debug/deps/daemon-tools-test");

        assert_bridge_artifact_selection(BridgeArtifactExpectation {
            executable,
            target_dir: Path::new("/"),
            configured_target_dir: Some(Path::new("/")),
            default_target_dir: Path::new("synthetic-default-target"),
            debug_profile: CARGO_TEST_PROFILE,
            expected_profile: CARGO_TEST_PROFILE,
            expected_target: None,
            recognized_target: None,
        });
    }

    #[test]
    fn bridge_default_target_recognition_accepts_a_filesystem_root() {
        reject_unrecognized_default_target(DefaultTargetRecognition {
            current_executable: Path::new("/debug/deps/daemon-tools-test"),
            configured_target_dir: None,
            default_target_dir: Path::new("/"),
            artifact_target_dir: None,
            known_targets: &BTreeSet::new(),
        });
    }

    #[test]
    fn bridge_rustc_command_resolves_a_relative_override_from_the_invocation_directory() {
        assert_eq!(
            rustc_command_for(
                Some(OsStr::new("tooling/rustc-wrapper")),
                Some(Path::new("synthetic-workspace")),
            ),
            PathBuf::from("synthetic-workspace/tooling/rustc-wrapper")
        );
    }

    #[test]
    fn bridge_rustc_command_uses_path_rustc_when_the_invocation_directory_is_unknown() {
        assert_eq!(
            rustc_command_for(Some(OsStr::new("tooling/rustc-wrapper")), None),
            PathBuf::from("rustc")
        );
    }

    #[test]
    fn bridge_compiler_wrapper_resolves_relative_to_the_invocation_directory() {
        assert_eq!(
            compiler_wrapper_command_for(
                OsStr::new("tooling/compiler-wrapper"),
                Some(Path::new("synthetic-workspace")),
            ),
            Some(PathBuf::from(
                "synthetic-workspace/tooling/compiler-wrapper"
            ))
        );
    }

    #[test]
    fn bridge_compiler_wrapper_rejects_a_relative_path_without_provenance() {
        assert_eq!(
            compiler_wrapper_command_for(OsStr::new("tooling/compiler-wrapper"), None),
            None
        );
    }

    #[test]
    fn bridge_compiler_wrapper_removes_an_empty_cache_bypass_override() {
        assert_eq!(
            compiler_wrapper_command_for(OsStr::new(""), Some(Path::new("synthetic-workspace"))),
            None
        );
    }

    #[test]
    fn bridge_rustc_command_preserves_a_bare_program_name_for_path_lookup() {
        assert_eq!(
            rustc_command_for(
                Some(OsStr::new("rustc")),
                Some(Path::new("synthetic-workspace"))
            ),
            PathBuf::from("rustc")
        );
    }

    #[test]
    fn bridge_compiler_wrapper_preserves_a_bare_program_name_for_path_lookup() {
        assert_eq!(
            compiler_wrapper_command_for(
                OsStr::new("sccache"),
                Some(Path::new("synthetic-workspace")),
            ),
            Some(PathBuf::from("sccache"))
        );
    }

    #[test]
    fn bridge_compiler_wrapper_keeps_resolving_an_explicit_current_directory_path() {
        assert_eq!(
            compiler_wrapper_command_for(
                OsStr::new("./sccache"),
                Some(Path::new("synthetic-workspace")),
            ),
            Some(PathBuf::from("synthetic-workspace/./sccache"))
        );
    }

    #[test]
    fn bridge_artifact_selection_resolves_a_parent_only_relative_directory() {
        let target_dir = Path::new("synthetic-parent");
        let executable = target_dir.join("debug/deps/daemon-tools-test");
        let configured_target_dir =
            configured_cargo_target_dir_for(ConfiguredCargoTargetDirInput {
                current_executable: &executable,
                configured: Path::new(".."),
                known_targets: &BTreeSet::new(),
            });

        assert_bridge_artifact_selection(BridgeArtifactExpectation {
            executable: &executable,
            target_dir,
            configured_target_dir: Some(&configured_target_dir),
            default_target_dir: Path::new("synthetic-default-target"),
            debug_profile: CARGO_TEST_PROFILE,
            expected_profile: CARGO_TEST_PROFILE,
            expected_target: None,
            recognized_target: None,
        });
    }

    #[test]
    fn bridge_artifact_selection_preserves_a_target_with_a_parent_only_directory() {
        let target_dir = Path::new("synthetic-parent");
        let executable = target_dir
            .join(SYNTHETIC_CARGO_TARGET)
            .join("debug/deps/daemon-tools-test");
        let configured_target_dir =
            configured_cargo_target_dir_for(ConfiguredCargoTargetDirInput {
                current_executable: &executable,
                configured: Path::new(".."),
                known_targets: &synthetic_known_targets(),
            });

        assert_bridge_artifact_selection(BridgeArtifactExpectation {
            executable: &executable,
            target_dir,
            configured_target_dir: Some(&configured_target_dir),
            default_target_dir: Path::new("synthetic-default-target"),
            debug_profile: CARGO_TEST_PROFILE,
            expected_profile: CARGO_TEST_PROFILE,
            expected_target: Some(SYNTHETIC_CARGO_TARGET),
            recognized_target: Some(SYNTHETIC_CARGO_TARGET),
        });
    }

    const BRIDGE_BUILD_TIMEOUT: Duration = Duration::from_secs(5 * 60);
    const BRIDGE_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(30);
    const BRIDGE_RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);
    const BRIDGE_EXIT_TIMEOUT: Duration = Duration::from_secs(12);
    const BRIDGE_CHILD_TEST_TIMEOUT: Duration = Duration::from_millis(25);
    const CHILD_POLL_INTERVAL: Duration = Duration::from_millis(10);
    const BRIDGE_WAIT_CHILD_FIXTURE_LIFETIME: Duration = Duration::from_secs(30);
    #[cfg(target_os = "linux")]
    const BRIDGE_STDOUT_DESCENDANT_CLEANUP_LIMIT: Duration = Duration::from_secs(15);
    const BRIDGE_WAIT_DESCENDANT_FIXTURE_LIFETIME: Duration = Duration::from_secs(30);
    #[cfg(target_os = "linux")]
    const SYNTHETIC_BLOCKING_DESCRIPTION_FRAGMENT: &str = "synthetic-padding";
    #[cfg(target_os = "linux")]
    const SYNTHETIC_BLOCKING_DESCRIPTION_REPETITIONS: usize = 300_000;
    static BRIDGE_BUILD_LOCK: Mutex<()> = Mutex::new(());

    struct BoundedCommandOutput {
        status: ExitStatus,
        stdout: Vec<u8>,
    }

    fn bounded_command_output(
        command: &mut Command,
        timeout: Duration,
    ) -> io::Result<Option<BoundedCommandOutput>> {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        configure_owned_process_group(command);
        let mut child = command.spawn()?;
        let mut stdout = child
            .stdout
            .take()
            .expect("bounded command stdout is piped");
        let reader = thread::spawn(move || {
            let mut bytes = Vec::new();
            stdout.read_to_end(&mut bytes)?;
            Ok::<_, io::Error>(bytes)
        });
        let status = match wait_for_owned_process_group(&mut child, timeout) {
            Ok(Some(status)) => status,
            Ok(None) => {
                kill_owned_process_group(&child);
                terminate_child(&mut child);
                join_bounded_stdout(reader)?;
                return Ok(None);
            }
            Err(error) => {
                terminate_owned_process_group(&mut child);
                let _ = join_bounded_stdout(reader);
                return Err(error);
            }
        };
        let stdout = join_bounded_stdout(reader)?;
        Ok(Some(BoundedCommandOutput { status, stdout }))
    }

    fn join_bounded_stdout(reader: JoinHandle<io::Result<Vec<u8>>>) -> io::Result<Vec<u8>> {
        reader
            .join()
            .map_err(|_| io::Error::other("bounded command stdout reader panicked"))?
    }

    fn wait_for_child(child: &mut Child, timeout: Duration) -> io::Result<Option<ExitStatus>> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = child.try_wait()? {
                return Ok(Some(status));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            thread::sleep(CHILD_POLL_INTERVAL);
        }
    }

    #[cfg(target_os = "linux")]
    fn wait_for_owned_process_group(
        child: &mut Child,
        timeout: Duration,
    ) -> io::Result<Option<ExitStatus>> {
        let pid = rustix::process::Pid::from_raw(child.id() as i32)
            .ok_or_else(|| io::Error::other("owned child has an invalid process id"))?;
        let deadline = Instant::now() + timeout;
        loop {
            let status = rustix::process::waitid(
                rustix::process::WaitId::Pid(pid),
                rustix::process::WaitIdOptions::EXITED
                    | rustix::process::WaitIdOptions::NOHANG
                    | rustix::process::WaitIdOptions::NOWAIT,
            )?;
            if status.is_some() {
                kill_owned_process_group(child);
                return child.wait().map(Some);
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            thread::sleep(CHILD_POLL_INTERVAL);
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn wait_for_owned_process_group(
        child: &mut Child,
        timeout: Duration,
    ) -> io::Result<Option<ExitStatus>> {
        wait_for_child(child, timeout)
    }

    fn terminate_child(child: &mut Child) {
        let _ = child.kill();
        let _ = child.wait();
    }

    fn configure_owned_process_group(command: &mut Command) {
        #[cfg(unix)]
        command.process_group(0);
        #[cfg(not(unix))]
        let _ = command;
    }

    fn kill_owned_process_group(child: &Child) {
        #[cfg(unix)]
        if let Some(group) = rustix::process::Pid::from_raw(child.id() as i32) {
            let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
        }
        #[cfg(not(unix))]
        let _ = child;
    }

    fn terminate_owned_process_group(child: &mut Child) -> bool {
        match child.try_wait() {
            Ok(None) => {
                kill_owned_process_group(child);
                terminate_child(child);
                true
            }
            Ok(Some(_)) | Err(_) => {
                terminate_child(child);
                false
            }
        }
    }

    #[test]
    #[ignore = "subprocess fixture for the bounded child-wait regression test"]
    fn bridge_wait_child_fixture() {
        thread::sleep(BRIDGE_WAIT_CHILD_FIXTURE_LIFETIME);
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "subprocess fixture for direct libtest invocation rejection"]
    fn bridge_build_direct_invocation_fixture() {
        assert!(ensure_claude_mcp_bridge_executable().is_file());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn bridge_build_rejects_an_ambiguous_directly_invoked_test_binary() {
        let current = std::env::current_exe().expect("test executable path is available");
        let invocation_directory =
            tempfile::tempdir().expect("synthetic direct invocation directory exists");
        let output = Command::new(current)
            .arg("daemon_tools::tests::bridge_build_direct_invocation_fixture")
            .args(["--exact", "--ignored"])
            .current_dir(invocation_directory.path())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .expect("directly invoked bridge-build fixture exits");
        let stdout = String::from_utf8_lossy(&output.stdout);

        assert!(!output.status.success());
        assert!(
            stdout.contains(
                "direct Cargo test artifacts must retain an unambiguous profile directory"
            )
        );
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "subprocess fixture that holds its parent's stderr open"]
    fn bridge_wait_descendant_fixture() {
        thread::sleep(BRIDGE_WAIT_DESCENDANT_FIXTURE_LIFETIME);
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "subprocess fixture that records and waits for a descendant"]
    fn bridge_wait_child_with_descendant_fixture() {
        let current = std::env::current_exe().expect("test executable path is available");
        let descendant = Command::new(current)
            .arg("daemon_tools::tests::bridge_wait_descendant_fixture")
            .args(["--exact", "--ignored"])
            .stdin(Stdio::null())
            .spawn()
            .expect("bounded-wait descendant starts");
        eprintln!("{}", descendant.id());
        io::stderr()
            .flush()
            .expect("descendant identity is flushed");
        descendant
            .wait_with_output()
            .expect("bounded-wait descendant is observed");
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "subprocess fixture that exits after leaving a stdout descendant"]
    #[expect(
        clippy::zombie_processes,
        reason = "the parent must exit without waiting so the bounded caller proves group cleanup"
    )]
    fn bridge_wait_child_leaving_stdout_descendant_fixture() {
        let current = std::env::current_exe().expect("test executable path is available");
        Command::new(current)
            .arg("daemon_tools::tests::bridge_wait_child_fixture")
            .args(["--exact", "--ignored"])
            .spawn()
            .expect("stdout descendant starts");
    }

    #[test]
    fn wait_for_child_returns_none_at_its_deadline_and_cleanup_reaps() {
        let current = std::env::current_exe().expect("test executable path is available");
        let mut child_command = Command::new(current);
        child_command
            .arg("daemon_tools::tests::bridge_wait_child_fixture")
            .args(["--exact", "--ignored"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        configure_owned_process_group(&mut child_command);
        let mut child = child_command.spawn().expect("bounded-wait child starts");

        let wait = wait_for_child(&mut child, BRIDGE_CHILD_TEST_TIMEOUT);
        terminate_owned_process_group(&mut child);

        assert!(
            wait.expect("bounded wait observes the live child")
                .is_none()
        );
        assert!(
            child
                .try_wait()
                .expect("cleaned child status is readable")
                .is_some()
        );
    }

    #[test]
    fn bounded_command_output_stops_a_stalled_inventory_process() {
        let current = std::env::current_exe().expect("test executable path is available");
        let mut command = Command::new(current);
        command
            .arg("daemon_tools::tests::bridge_wait_child_fixture")
            .args(["--exact", "--ignored"]);

        assert!(
            bounded_command_output(&mut command, BRIDGE_CHILD_TEST_TIMEOUT)
                .expect("bounded inventory command is observed")
                .is_none()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn bounded_command_output_cleans_up_stdout_descendants_after_parent_exit() {
        let current = std::env::current_exe().expect("test executable path is available");
        let mut command = Command::new(current);
        command
            .arg("daemon_tools::tests::bridge_wait_child_leaving_stdout_descendant_fixture")
            .args(["--exact", "--ignored"]);
        let started = Instant::now();

        assert!(
            bounded_command_output(&mut command, BRIDGE_EXIT_TIMEOUT)
                .expect("bounded command observes the exited parent")
                .is_some()
        );
        assert!(started.elapsed() < BRIDGE_STDOUT_DESCENDANT_CLEANUP_LIMIT);
    }

    #[test]
    fn owned_process_group_cleanup_skips_a_reaped_child() {
        let current = std::env::current_exe().expect("test executable path is available");
        let mut child_command = Command::new(current);
        child_command
            .arg("--help")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        configure_owned_process_group(&mut child_command);
        let mut child = child_command.spawn().expect("short-lived child starts");
        let status = child.wait().expect("short-lived child is reaped");

        assert!(status.success());
        assert!(!terminate_owned_process_group(&mut child));
    }

    #[cfg(unix)]
    #[test]
    fn owned_process_group_cleanup_terminates_a_descendant_holding_stderr() {
        let current = std::env::current_exe().expect("test executable path is available");
        let mut child_command = Command::new(current);
        child_command
            .arg("daemon_tools::tests::bridge_wait_child_with_descendant_fixture")
            .args(["--exact", "--ignored", "--nocapture"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        configure_owned_process_group(&mut child_command);
        let mut child = child_command
            .spawn()
            .expect("bounded-wait child with descendant starts");
        let child_pid = child.id();
        let (descendant_output, reader) = response_reader(BufReader::new(
            child.stderr.take().expect("bounded-wait stderr is piped"),
        ));
        let descendant_pid = descendant_output
            .recv_timeout(BRIDGE_RESPONSE_TIMEOUT)
            .expect("descendant identity arrives")
            .expect("descendant identity read succeeds")
            .trim()
            .parse::<u32>()
            .expect("descendant identity is a process id");

        assert_ne!(descendant_pid, child_pid);
        assert!(
            wait_for_child(&mut child, BRIDGE_CHILD_TEST_TIMEOUT)
                .expect("bounded wait observes the live child")
                .is_none()
        );
        terminate_owned_process_group(&mut child);
        assert!(matches!(
            descendant_output.recv_timeout(BRIDGE_RESPONSE_TIMEOUT),
            Err(RecvTimeoutError::Disconnected)
        ));
        reader.join().expect("descendant stderr reader exits");
    }

    #[cfg(target_os = "linux")]
    #[track_caller]
    fn ensure_claude_mcp_bridge_executable() -> PathBuf {
        let _build_guard = BRIDGE_BUILD_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("signalboxd manifest has a workspace root")
            .to_path_buf();
        let invocation = current_cargo_test_invocation();
        let selection = claude_mcp_bridge_artifact_selection(&invocation);
        require_direct_bridge_execution(&selection);
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
        let mut build_command = Command::new(cargo);
        apply_cargo_unstable_flags(&mut build_command, &invocation.unstable_flags);
        build_command
            .args([
                "build",
                "--offline",
                "-p",
                "signalbox-model-runtime-claude-cli",
                "--bin",
                CLAUDE_MCP_BRIDGE_BINARY,
                "--message-format=json-render-diagnostics",
            ])
            .arg("--profile")
            .arg(&selection.profile)
            .arg("--target-dir")
            .arg(&selection.target_dir)
            .stdout(Stdio::piped());
        configure_bridge_build_location(
            &mut build_command,
            BridgeBuildLocation {
                invocation_directory: &invocation.invocation_directory,
                workspace: &workspace,
            },
        );
        apply_cargo_config_overrides(&mut build_command, &invocation.config_overrides);
        apply_cargo_rust_version_policy(&mut build_command, invocation.ignore_rust_version);
        if let Some(rustc) = normalized_rustc_override(&invocation.invocation_directory) {
            build_command.env("RUSTC", rustc);
        }
        configure_compiler_wrapper(
            &mut build_command,
            "RUSTC_WRAPPER",
            &invocation.invocation_directory,
        );
        configure_compiler_wrapper(
            &mut build_command,
            "RUSTC_WORKSPACE_WRAPPER",
            &invocation.invocation_directory,
        );
        if let Some(target) = &selection.target {
            build_command.arg("--target").arg(target);
        }
        configure_owned_process_group(&mut build_command);
        let mut build = build_command.spawn().expect("bridge binary build starts");
        let (messages, reader) = response_reader(BufReader::new(
            build.stdout.take().expect("Cargo build stdout is piped"),
        ));
        let Some(status) = wait_for_owned_process_group(&mut build, BRIDGE_BUILD_TIMEOUT)
            .expect("bridge binary build is observed")
        else {
            kill_owned_process_group(&build);
            terminate_owned_process_group(&mut build);
            reader.join().expect("Cargo build output reader exits");
            panic!("bridge binary build exceeded its timeout");
        };
        reader.join().expect("Cargo build output reader exits");
        assert!(status.success(), "bridge binary build succeeds");
        let executable = cargo_bridge_executable(messages);
        assert!(
            executable.is_file(),
            "bridge binary build produces its target"
        );
        executable
    }

    fn apply_cargo_config_overrides(command: &mut Command, config_overrides: &[OsString]) {
        for config in config_overrides {
            command.arg(CARGO_CONFIG_OPTION).arg(config);
        }
    }

    fn apply_cargo_rust_version_policy(command: &mut Command, ignore_rust_version: bool) {
        if ignore_rust_version {
            command.arg(CARGO_IGNORE_RUST_VERSION_OPTION);
        }
    }

    fn configure_bridge_build_location(command: &mut Command, location: BridgeBuildLocation<'_>) {
        command
            .arg(CARGO_MANIFEST_PATH_OPTION)
            .arg(location.workspace.join(CARGO_MANIFEST_FILENAME))
            .current_dir(location.invocation_directory);
    }

    #[track_caller]
    fn require_direct_bridge_execution(selection: &BridgeArtifactSelection) {
        assert!(
            selection.target.is_none(),
            "target-specific bridge execution is unsupported because Cargo runner semantics cannot be preserved"
        );
    }

    #[test]
    #[should_panic(
        expected = "target-specific bridge execution is unsupported because Cargo runner semantics cannot be preserved"
    )]
    fn bridge_build_rejects_target_specific_execution_before_launch() {
        require_direct_bridge_execution(&BridgeArtifactSelection {
            profile: OsString::from(CARGO_TEST_PROFILE),
            target: Some(OsString::from(SYNTHETIC_CARGO_TARGET)),
            target_dir: PathBuf::from("synthetic-target"),
        });
    }

    #[track_caller]
    fn cargo_bridge_executable(messages: Receiver<Result<String, io::Error>>) -> PathBuf {
        messages
            .into_iter()
            .map(|message| message.expect("Cargo build output is readable"))
            .map(|message| {
                serde_json::from_str::<serde_json::Value>(&message)
                    .expect("Cargo build output is JSON")
            })
            .find_map(|message| cargo_bridge_executable_from_message(&message))
            .expect("Cargo reports the bridge executable artifact")
    }

    fn cargo_bridge_executable_from_message(message: &serde_json::Value) -> Option<PathBuf> {
        (message["reason"] == "compiler-artifact"
            && message["target"]["name"] == CLAUDE_MCP_BRIDGE_BINARY)
            .then(|| message["executable"].as_str())
            .flatten()
            .map(PathBuf::from)
    }

    #[test]
    fn cargo_bridge_artifact_uses_the_reported_executable_path() {
        const SYNTHETIC_REPORTED_EXECUTABLE: &str = "synthetic-target/bridge";
        let executable = PathBuf::from(SYNTHETIC_REPORTED_EXECUTABLE);
        let message = serde_json::json!({
            "reason": "compiler-artifact",
            "target": {"name": CLAUDE_MCP_BRIDGE_BINARY},
            "executable": executable,
        });

        assert_eq!(
            cargo_bridge_executable_from_message(&message),
            Some(executable)
        );
    }

    #[track_caller]
    fn response_reader<Output>(
        mut output: Output,
    ) -> (Receiver<Result<String, io::Error>>, JoinHandle<()>)
    where
        Output: BufRead + Send + 'static,
    {
        let (sender, receiver) = mpsc::channel();
        let reader = thread::spawn(move || {
            loop {
                let mut response = String::new();
                match output.read_line(&mut response) {
                    Ok(0) => return,
                    Ok(_) => {
                        if sender.send(Ok(response)).is_err() {
                            return;
                        }
                    }
                    Err(error) => {
                        let _ = sender.send(Err(error));
                        return;
                    }
                }
            }
        });
        (receiver, reader)
    }

    #[track_caller]
    fn bridge_response_reader(
        output: ChildStdout,
    ) -> (Receiver<Result<String, io::Error>>, JoinHandle<()>) {
        response_reader(BufReader::new(output))
    }

    struct OneLineThenPanic {
        consumed: bool,
        read_started: Option<SyncSender<()>>,
        release_read: Receiver<()>,
    }

    fn bridge_response_line() -> &'static str {
        "response\n"
    }

    impl Read for OneLineThenPanic {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let available = self.fill_buf()?;
            let length = available.len().min(buffer.len());
            buffer[..length].copy_from_slice(&available[..length]);
            self.consume(length);
            Ok(length)
        }
    }

    impl BufRead for OneLineThenPanic {
        fn fill_buf(&mut self) -> io::Result<&[u8]> {
            assert!(!self.consumed, "reader is not polled after disconnection");
            self.read_started
                .take()
                .expect("first read is signalled once")
                .send(())
                .expect("read-start receiver remains connected");
            self.release_read
                .recv()
                .expect("first read is released by the fixture");
            Ok(bridge_response_line().as_bytes())
        }

        fn consume(&mut self, _amount: usize) {
            self.consumed = true;
        }
    }

    const SYNTHETIC_BRIDGE_READER_ERROR_KIND: ErrorKind = ErrorKind::BrokenPipe;
    const SYNTHETIC_BRIDGE_READER_ERROR_MESSAGE: &str = "synthetic failure";

    struct FailingBridgeReader;

    impl Read for FailingBridgeReader {
        fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::new(
                SYNTHETIC_BRIDGE_READER_ERROR_KIND,
                SYNTHETIC_BRIDGE_READER_ERROR_MESSAGE,
            ))
        }
    }

    impl BufRead for FailingBridgeReader {
        fn fill_buf(&mut self) -> io::Result<&[u8]> {
            Err(io::Error::new(
                SYNTHETIC_BRIDGE_READER_ERROR_KIND,
                SYNTHETIC_BRIDGE_READER_ERROR_MESSAGE,
            ))
        }

        fn consume(&mut self, _amount: usize) {}
    }

    #[test]
    fn bridge_response_reader_delivers_one_complete_line() {
        let expected_response = bridge_response_line();
        let (responses, reader) = response_reader(Cursor::new(expected_response.as_bytes()));

        assert_eq!(
            responses
                .recv_timeout(BRIDGE_RESPONSE_TIMEOUT)
                .expect("response arrives")
                .expect("response read succeeds"),
            expected_response
        );
        reader.join().expect("response reader exits");
    }

    #[test]
    fn bridge_response_reader_closes_at_eof() {
        let (responses, reader) = response_reader(Cursor::new(Vec::<u8>::new()));

        assert!(matches!(
            responses.recv_timeout(BRIDGE_RESPONSE_TIMEOUT),
            Err(RecvTimeoutError::Disconnected)
        ));
        reader.join().expect("response reader exits");
    }

    #[test]
    fn bridge_response_reader_stops_when_its_receiver_disconnects() {
        let (read_started, read_started_receiver) = mpsc::sync_channel(0);
        let (release_read, release_read_receiver) = mpsc::sync_channel(0);
        let (responses, reader) = response_reader(OneLineThenPanic {
            consumed: false,
            read_started: Some(read_started),
            release_read: release_read_receiver,
        });
        read_started_receiver
            .recv_timeout(BRIDGE_RESPONSE_TIMEOUT)
            .expect("response reader starts its first read");
        drop(responses);
        release_read
            .send(())
            .expect("blocked first read remains connected");

        reader
            .join()
            .expect("response reader exits after disconnect");
    }

    #[test]
    fn bridge_response_reader_forwards_a_read_failure() {
        let (responses, reader) = response_reader(FailingBridgeReader);

        let error = responses
            .recv_timeout(BRIDGE_RESPONSE_TIMEOUT)
            .expect("failure arrives")
            .expect_err("read failure is preserved");
        assert_eq!(error.kind(), SYNTHETIC_BRIDGE_READER_ERROR_KIND);
        reader.join().expect("response reader exits");
    }

    struct McpBridgeProcess {
        child: Child,
        input: Option<ChildStdin>,
        responses: Receiver<Result<String, io::Error>>,
        reader: Option<JoinHandle<()>>,
    }

    struct McpBridgeSpawn<'a> {
        executable: &'a Path,
        catalog: &'a Path,
        ready: &'a Path,
        workspace: &'a Path,
    }

    impl McpBridgeProcess {
        #[track_caller]
        fn spawn(config: McpBridgeSpawn<'_>) -> Self {
            let mut child = Command::new(config.executable)
                .arg(CLAUDE_MCP_BRIDGE_SERVE_OPTION)
                .arg(config.catalog)
                .arg(config.ready)
                .current_dir(config.workspace)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .expect("Claude MCP bridge binary starts");
            let input = child.stdin.take().expect("bridge stdin is piped");
            let (responses, reader) =
                bridge_response_reader(child.stdout.take().expect("bridge stdout is piped"));
            Self {
                child,
                input: Some(input),
                responses,
                reader: Some(reader),
            }
        }

        #[track_caller]
        fn request(&mut self, request: &serde_json::Value) -> serde_json::Value {
            let request_id = request
                .get("id")
                .expect("MCP request has an identity")
                .clone();
            let response = self.raw_response(request);
            let response = serde_json::from_str(&response).expect("MCP response is JSON");
            assert!(
                valid_mcp_response_envelope(McpResponseEnvelope {
                    response: &response,
                    request_id: &request_id,
                }),
                "MCP response has the exact JSON-RPC version and request identity"
            );
            response
        }

        #[track_caller]
        fn raw_response(&mut self, request: &serde_json::Value) -> String {
            let input = self.input.as_mut().expect("bridge stdin remains open");
            serde_json::to_writer(&mut *input, request).expect("MCP request serializes");
            input.write_all(b"\n").expect("MCP request is written");
            input.flush().expect("MCP request is flushed");
            match self.responses.recv_timeout(BRIDGE_RESPONSE_TIMEOUT) {
                Ok(Ok(response)) => response,
                Ok(Err(error)) => panic!("MCP response read failed: {error}"),
                Err(RecvTimeoutError::Disconnected) => {
                    panic!("MCP bridge closed stdout before responding")
                }
                Err(RecvTimeoutError::Timeout) => {
                    terminate_child(&mut self.child);
                    panic!("MCP bridge response exceeded its timeout")
                }
            }
        }

        #[track_caller]
        fn notify(&mut self, notification: &serde_json::Value) {
            let input = self.input.as_mut().expect("bridge stdin remains open");
            serde_json::to_writer(&mut *input, notification).expect("MCP notification serializes");
            input.write_all(b"\n").expect("MCP notification is written");
            input.flush().expect("MCP notification is flushed");
        }

        #[track_caller]
        fn finish(mut self) {
            drop(self.input.take());
            let Some(status) = wait_for_child(&mut self.child, BRIDGE_EXIT_TIMEOUT)
                .expect("bridge process exit is observed")
            else {
                terminate_child(&mut self.child);
                panic!("MCP bridge exit exceeded its timeout");
            };
            self.join_reader();
            assert!(status.success());
        }

        #[track_caller]
        fn join_reader(&mut self) {
            if let Some(reader) = self.reader.take() {
                reader.join().expect("bridge response reader exits");
            }
        }
    }

    struct McpResponseEnvelope<'a> {
        response: &'a serde_json::Value,
        request_id: &'a serde_json::Value,
    }

    fn valid_mcp_response_envelope(envelope: McpResponseEnvelope<'_>) -> bool {
        let has_result = envelope.response.get("result").is_some();
        let has_error = envelope.response.get("error").is_some();
        envelope
            .response
            .get("jsonrpc")
            .and_then(serde_json::Value::as_str)
            == Some(MCP_JSON_RPC_VERSION)
            && envelope.response.get("id") == Some(envelope.request_id)
            && has_result != has_error
    }

    #[test]
    fn mcp_response_envelope_rejects_a_wrong_protocol_version() {
        let request_id = serde_json::json!(MCP_ENVELOPE_REQUEST_ID);
        let response = serde_json::json!({
            "jsonrpc": SYNTHETIC_WRONG_JSON_RPC_VERSION,
            "id": request_id.clone(),
            "result": {},
        });

        assert!(!valid_mcp_response_envelope(McpResponseEnvelope {
            response: &response,
            request_id: &request_id,
        }));
    }

    #[test]
    fn mcp_response_envelope_rejects_a_mismatched_request_identity() {
        let request_id = serde_json::json!(MCP_ENVELOPE_REQUEST_ID);
        let response = serde_json::json!({
            "jsonrpc": MCP_JSON_RPC_VERSION,
            "id": MCP_OTHER_REQUEST_ID,
            "result": {},
        });

        assert!(!valid_mcp_response_envelope(McpResponseEnvelope {
            response: &response,
            request_id: &request_id,
        }));
    }

    #[test]
    fn mcp_response_envelope_rejects_result_and_error_together() {
        let request_id = serde_json::json!(MCP_ENVELOPE_REQUEST_ID);
        let response = serde_json::json!({
            "jsonrpc": MCP_JSON_RPC_VERSION,
            "id": request_id.clone(),
            "result": {},
            "error": {
                "code": SYNTHETIC_JSON_RPC_ERROR_CODE,
                "message": SYNTHETIC_JSON_RPC_ERROR_MESSAGE,
            },
        });

        assert!(!valid_mcp_response_envelope(McpResponseEnvelope {
            response: &response,
            request_id: &request_id,
        }));
    }

    #[test]
    fn raw_list_response_rejects_result_and_error_together() {
        let response = format!(
            r#"{{"jsonrpc":"{MCP_JSON_RPC_VERSION}","id":{MCP_LIST_TOOLS_REQUEST_ID},"result":{{"tools":[]}},"error":{{"code":{SYNTHETIC_JSON_RPC_ERROR_CODE},"message":"{SYNTHETIC_JSON_RPC_ERROR_MESSAGE}"}}}}"#
        );
        let response: ListedBridgeResponse =
            serde_json::from_str(&response).expect("synthetic list response is valid JSON");

        assert_eq!(response.into_tools(MCP_LIST_TOOLS_REQUEST_ID), None);
    }

    #[test]
    fn raw_list_response_rejects_result_and_null_error_together() {
        let response = format!(
            r#"{{"jsonrpc":"{MCP_JSON_RPC_VERSION}","id":{MCP_LIST_TOOLS_REQUEST_ID},"result":{{"tools":[]}},"error":null}}"#
        );
        let response: ListedBridgeResponse =
            serde_json::from_str(&response).expect("synthetic list response is valid JSON");

        assert_eq!(response.into_tools(MCP_LIST_TOOLS_REQUEST_ID), None);
    }

    #[test]
    fn raw_list_response_compares_a_deep_schema_semantically() {
        let schema = synthetic_deep_bridge_tool_schema();
        let response = format!(
            r#"{{"jsonrpc":"{MCP_JSON_RPC_VERSION}","id":{MCP_LIST_TOOLS_REQUEST_ID},"result":{{"tools":[{{"name":"{SYNTHETIC_BRIDGE_TOOL_NAME}","description":"{SYNTHETIC_BRIDGE_TOOL_DESCRIPTION}","inputSchema":{schema}}}]}}}}"#
        );
        let response: ListedBridgeResponse =
            serde_json::from_str(&response).expect("deep synthetic list response is valid JSON");
        let expected_schema =
            ToolInputSchema::try_new(schema).expect("deep synthetic schema is admitted");

        assert_eq!(
            response.into_tools(MCP_LIST_TOOLS_REQUEST_ID),
            Some(vec![ComparableBridgeTool {
                name: String::from(SYNTHETIC_BRIDGE_TOOL_NAME),
                description: String::from(SYNTHETIC_BRIDGE_TOOL_DESCRIPTION),
                input_schema: expected_schema,
            }])
        );
    }

    #[test]
    fn raw_list_response_rejects_an_unmodeled_tool_member() {
        let response = serde_json::json!({
            "jsonrpc": MCP_JSON_RPC_VERSION,
            "id": MCP_LIST_TOOLS_REQUEST_ID,
            "result": {
                "tools": [{
                    "name": SYNTHETIC_BRIDGE_TOOL_NAME,
                    "description": SYNTHETIC_BRIDGE_TOOL_DESCRIPTION,
                    "inputSchema": serde_json::from_str::<serde_json::Value>(
                        SYNTHETIC_BRIDGE_TOOL_SCHEMA
                    )
                    .expect("the synthetic bridge schema is valid JSON"),
                    "title": SYNTHETIC_UNMODELED_BRIDGE_TOOL_TITLE,
                }]
            }
        })
        .to_string();

        assert!(serde_json::from_str::<ListedBridgeResponse>(&response).is_err());
    }

    #[test]
    fn raw_list_response_rejects_pagination_metadata() {
        let response = serde_json::json!({
            "jsonrpc": MCP_JSON_RPC_VERSION,
            "id": MCP_LIST_TOOLS_REQUEST_ID,
            "result": {
                "tools": [],
                "nextCursor": SYNTHETIC_MCP_NEXT_CURSOR,
            }
        })
        .to_string();

        assert!(serde_json::from_str::<ListedBridgeResponse>(&response).is_err());
    }

    #[test]
    fn mcp_response_envelope_rejects_neither_result_nor_error() {
        let request_id = serde_json::json!(MCP_ENVELOPE_REQUEST_ID);
        let response = serde_json::json!({
            "jsonrpc": MCP_JSON_RPC_VERSION,
            "id": request_id.clone(),
        });

        assert!(!valid_mcp_response_envelope(McpResponseEnvelope {
            response: &response,
            request_id: &request_id,
        }));
    }

    impl Drop for McpBridgeProcess {
        fn drop(&mut self) {
            if self.child.try_wait().ok().flatten().is_none() {
                terminate_child(&mut self.child);
            }
            self.join_reader();
        }
    }

    #[cfg(target_os = "linux")]
    struct McpBridgeReadyWaiter {
        child: Child,
    }

    #[cfg(target_os = "linux")]
    struct McpBridgeReadyWaiterSpawn<'a> {
        executable: &'a Path,
        ready: &'a Path,
        workspace: &'a Path,
    }

    #[cfg(target_os = "linux")]
    impl McpBridgeReadyWaiter {
        #[track_caller]
        fn start(config: McpBridgeReadyWaiterSpawn<'_>) -> Self {
            let child = Command::new(config.executable)
                .arg(CLAUDE_MCP_BRIDGE_WAIT_READY_OPTION)
                .arg(config.ready)
                .current_dir(config.workspace)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("bridge readiness waiter starts");
            Self { child }
        }

        #[track_caller]
        fn synchronize_wait_path(&mut self) {
            let status_path = Path::new(PROC_FILESYSTEM_ROOT)
                .join(self.child.id().to_string())
                .join(PROC_PROCESS_STAT_FILENAME);
            let deadline = Instant::now() + BRIDGE_RESPONSE_TIMEOUT;
            loop {
                let state = fs::read_to_string(&status_path).ok().and_then(|status| {
                    status
                        .rsplit_once(") ")
                        .and_then(|(_, fields)| fields.chars().next())
                });
                if state == Some('S') {
                    return;
                }
                if let Some(status) = self
                    .child
                    .try_wait()
                    .expect("bridge readiness waiter remains observable")
                {
                    panic!("bridge readiness waiter exited before blocking: {status}");
                }
                assert!(
                    Instant::now() < deadline,
                    "bridge readiness waiter enters its blocking sleep"
                );
                thread::sleep(CHILD_POLL_INTERVAL);
            }
        }

        #[track_caller]
        fn assert_blocks_before_listing(&mut self) {
            assert!(
                wait_for_child(&mut self.child, BRIDGE_CHILD_TEST_TIMEOUT)
                    .expect("bridge readiness waiter remains observable")
                    .is_none(),
                "bridge readiness waiter stays blocked before tools/list"
            );
        }

        #[track_caller]
        fn assert_blocks_while_list_response_is_backpressured(&mut self) {
            assert!(
                wait_for_child(&mut self.child, BRIDGE_CHILD_TEST_TIMEOUT)
                    .expect("bridge readiness waiter remains observable")
                    .is_none(),
                "bridge readiness waiter stays blocked before the full tools/list response"
            );
        }

        #[track_caller]
        fn finish_success(mut self) {
            let Some(status) = wait_for_child(&mut self.child, BRIDGE_EXIT_TIMEOUT)
                .expect("bridge readiness exit is observed")
            else {
                terminate_child(&mut self.child);
                panic!("bridge readiness wait exceeded its timeout");
            };
            assert!(
                status.success(),
                "bridge publishes readiness after listing tools"
            );
        }
    }

    #[cfg(target_os = "linux")]
    impl Drop for McpBridgeReadyWaiter {
        fn drop(&mut self) {
            if self.child.try_wait().ok().flatten().is_none() {
                terminate_child(&mut self.child);
            }
        }
    }

    #[cfg(target_os = "linux")]
    struct BlockingListResponseFixture {
        _support: tempfile::TempDir,
        executable: PathBuf,
        ready_path: PathBuf,
        child: Child,
        input: Option<ChildStdin>,
        output: Option<BufReader<ChildStdout>>,
    }

    #[cfg(target_os = "linux")]
    #[track_caller]
    fn write_blocking_bridge_message(input: &mut ChildStdin, message: &serde_json::Value) {
        serde_json::to_writer(&mut *input, message).expect("blocking bridge message serializes");
        input
            .write_all(b"\n")
            .expect("blocking bridge message is written");
        input.flush().expect("blocking bridge message is flushed");
    }

    #[cfg(target_os = "linux")]
    impl BlockingListResponseFixture {
        #[track_caller]
        fn start() -> Self {
            let support = tempfile::tempdir().expect("blocking response support root exists");
            let description = SYNTHETIC_BLOCKING_DESCRIPTION_FRAGMENT
                .repeat(SYNTHETIC_BLOCKING_DESCRIPTION_REPETITIONS);
            let definition = ToolDefinition::new(
                ToolName::try_new(String::from(SYNTHETIC_BRIDGE_TOOL_NAME))
                    .expect("synthetic bridge tool name is valid"),
                description,
                ToolInputSchema::try_new(String::from(SYNTHETIC_BRIDGE_TOOL_SCHEMA))
                    .expect("synthetic bridge schema is valid"),
                ToolPermissionDefault::Confirm,
                ToolEffectClass::EffectFree,
            );
            let catalog = bridge_catalog(&[definition]);
            let catalog_path = support.path().join(MCP_CATALOG_FILENAME);
            let ready_path = support.path().join(MCP_READY_FILENAME);
            fs::write(&catalog_path, &catalog.catalog).expect("blocking bridge catalog is written");
            let executable = catalog.executable;
            let mut command = Command::new(&executable);
            command
                .arg(CLAUDE_MCP_BRIDGE_SERVE_OPTION)
                .arg(&catalog_path)
                .arg(&ready_path)
                .current_dir(support.path())
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null());
            configure_owned_process_group(&mut command);
            let mut child = command.spawn().expect("blocking bridge starts");
            let mut input = child.stdin.take().expect("blocking bridge stdin is piped");
            write_blocking_bridge_message(
                &mut input,
                &serde_json::json!({
                    "jsonrpc": MCP_JSON_RPC_VERSION,
                    "id": MCP_INITIALIZE_REQUEST_ID,
                    "method": "initialize",
                    "params": {
                        "protocolVersion": MCP_PROTOCOL_VERSION,
                        "capabilities": {},
                        "clientInfo": {
                            "name": MCP_CLIENT_NAME,
                            "version": env!("CARGO_PKG_VERSION"),
                        },
                    },
                }),
            );
            write_blocking_bridge_message(
                &mut input,
                &serde_json::json!({
                    "jsonrpc": MCP_JSON_RPC_VERSION,
                    "method": "notifications/initialized",
                }),
            );
            write_blocking_bridge_message(
                &mut input,
                &serde_json::json!({
                    "jsonrpc": MCP_JSON_RPC_VERSION,
                    "id": MCP_BLOCKING_LIST_REQUEST_ID,
                    "method": "tools/list",
                    "params": {},
                }),
            );
            let output = BufReader::new(
                child
                    .stdout
                    .take()
                    .expect("blocking bridge stdout is piped"),
            );
            Self {
                _support: support,
                executable,
                ready_path,
                child,
                input: Some(input),
                output: Some(output),
            }
        }

        #[track_caller]
        fn await_list_response_started(&mut self) {
            let mut output = self
                .output
                .take()
                .expect("blocking bridge output is present");
            let (sender, receiver) = mpsc::sync_channel(1);
            let reader = thread::spawn(move || {
                let result = (|| -> io::Result<(BufReader<ChildStdout>, String)> {
                    let mut initialized = String::new();
                    output.read_line(&mut initialized)?;
                    if output.fill_buf()?.is_empty() {
                        return Err(io::Error::new(
                            ErrorKind::UnexpectedEof,
                            "bridge list response has no prefix",
                        ));
                    }
                    Ok((output, initialized))
                })();
                let _ = sender.send(result);
            });
            let (output, initialized) = match receiver.recv_timeout(BRIDGE_RESPONSE_TIMEOUT) {
                Ok(Ok(observed)) => observed,
                Ok(Err(error)) => {
                    terminate_owned_process_group(&mut self.child);
                    reader.join().expect("bounded prefix reader exits");
                    panic!("blocking bridge response prefix failed: {error}");
                }
                Err(error) => {
                    terminate_owned_process_group(&mut self.child);
                    reader.join().expect("bounded prefix reader exits");
                    panic!("blocking bridge response prefix exceeded its bound: {error}");
                }
            };
            reader.join().expect("bounded prefix reader exits");
            self.output = Some(output);
            let initialized: serde_json::Value =
                serde_json::from_str(&initialized).expect("blocking initialize response is JSON");
            assert!(valid_mcp_response_envelope(McpResponseEnvelope {
                response: &initialized,
                request_id: &serde_json::json!(MCP_INITIALIZE_REQUEST_ID),
            }));
        }

        #[track_caller]
        fn read_list_response(&mut self) -> serde_json::Value {
            let mut output = self
                .output
                .take()
                .expect("blocking bridge output is present");
            let (sender, receiver) = mpsc::sync_channel(1);
            let reader = thread::spawn(move || {
                let result = (|| -> io::Result<String> {
                    let mut listed = String::new();
                    output.read_line(&mut listed)?;
                    Ok(listed)
                })();
                let _ = sender.send(result);
            });
            let response = match receiver.recv_timeout(BRIDGE_RESPONSE_TIMEOUT) {
                Ok(Ok(response)) => response,
                Ok(Err(error)) => {
                    terminate_owned_process_group(&mut self.child);
                    reader.join().expect("bounded response reader exits");
                    panic!("blocking bridge response read failed: {error}");
                }
                Err(error) => {
                    terminate_owned_process_group(&mut self.child);
                    reader.join().expect("bounded response reader exits");
                    panic!("blocking bridge response exceeded its bound: {error}");
                }
            };
            reader.join().expect("bounded response reader exits");
            let response: serde_json::Value =
                serde_json::from_str(&response).expect("blocking list response is JSON");
            assert!(valid_mcp_response_envelope(McpResponseEnvelope {
                response: &response,
                request_id: &serde_json::json!(MCP_BLOCKING_LIST_REQUEST_ID),
            }));
            response
        }

        #[track_caller]
        fn finish(&mut self) {
            drop(self.input.take());
            let Some(status) = wait_for_child(&mut self.child, BRIDGE_EXIT_TIMEOUT)
                .expect("blocking bridge exit is observed")
            else {
                terminate_owned_process_group(&mut self.child);
                panic!("blocking bridge exit exceeded its timeout");
            };
            assert!(status.success(), "blocking bridge exits successfully");
        }
    }

    #[cfg(target_os = "linux")]
    impl Drop for BlockingListResponseFixture {
        fn drop(&mut self) {
            if self.child.try_wait().ok().flatten().is_none() {
                terminate_owned_process_group(&mut self.child);
            }
        }
    }

    #[test]
    fn composed_catalog_applies_an_enforceable_posture() {
        let (echo_catalog, _executor) = EchoTool::try_new()
            .expect("echo fixture compiles")
            .into_parts();
        let catalog = DaemonToolCatalog::try_new([echo_catalog])
            .expect("single-tool fixture has unique names");
        let echo = ToolName::try_new(String::from(ECHO_NAME)).expect("fixture name is valid");
        let configured = catalog
            .with_approval_postures([(echo.clone(), ToolApprovalPosture::Human)])
            .expect("known tool posture is applied");

        assert_eq!(
            configured
                .definition(&echo)
                .expect("configured tool remains present")
                .approval_posture(),
            Some(ToolApprovalPosture::Human)
        );
    }

    /// The shipped posture table and daemon catalog compose both egress tools
    /// into user-approved requests while their declarations stay fail-closed.
    #[test]
    fn shipped_web_postures_resolve_both_daemon_tools_to_human_approval() {
        let configuration = crate::configuration::checked_in_example_configuration()
            .expect("checked-in configuration is valid");
        let (web_fetch_catalog, _executor) =
            WebFetchTool::try_new(OfflineTransport, WebFetchEgressPolicy::deny_all())
                .expect("offline web fetch tool compiles")
                .into_parts();
        let (web_search_catalog, _executor) = WebSearchTool::try_new(
            OfflineCredentials,
            OfflineSearchTransport,
            WebSearchConfiguration::new(WebSearchProvider::Brave),
        )
        .expect("offline web search tool compiles")
        .into_parts();
        let catalog = DaemonToolCatalog::try_new([web_fetch_catalog, web_search_catalog])
            .expect("web tool names are distinct")
            .with_approval_postures(configuration.tool_approval_postures())
            .expect("shipped postures name composed tools");
        let web_fetch =
            ToolName::try_new(String::from(WEB_FETCH_NAME)).expect("web fetch name is valid");
        let web_search =
            ToolName::try_new(String::from(WEB_SEARCH_NAME)).expect("web search name is valid");
        let web_fetch_definition = catalog
            .definition(&web_fetch)
            .expect("web fetch remains composed");
        let web_search_definition = catalog
            .definition(&web_search)
            .expect("web search remains composed");

        assert_eq!(
            web_fetch_definition.approval_posture(),
            Some(ToolApprovalPosture::Human)
        );
        assert_eq!(
            web_search_definition.approval_posture(),
            Some(ToolApprovalPosture::Human)
        );
        assert_eq!(
            web_fetch_definition.permission_default(),
            ToolPermissionDefault::Confirm
        );
        assert_eq!(
            web_search_definition.permission_default(),
            ToolPermissionDefault::Confirm
        );
    }

    #[test]
    fn composed_catalog_rejects_an_unknown_posture_name() {
        let (echo_catalog, _executor) = EchoTool::try_new()
            .expect("echo fixture compiles")
            .into_parts();
        let catalog = DaemonToolCatalog::try_new([echo_catalog])
            .expect("single-tool fixture has unique names");
        let unknown = ToolName::try_new(String::from("unknown_tool"))
            .expect("unknown fixture name is structurally valid");
        let rejected = catalog
            .with_approval_postures([(unknown.clone(), ToolApprovalPosture::Human)])
            .expect_err("unknown tool posture fails closed");

        assert_eq!(rejected.name(), &unknown);
    }

    #[test]
    fn base_composition_prevalidation_rejects_an_uncomposed_mapped_tool() {
        let mapped = ToolName::try_new(String::from(PULL_REQUEST_METADATA_NAME))
            .expect("mapped fixture name is valid");
        let rejected = DaemonToolCatalog::validate_approval_postures_for_composition(
            [(mapped.clone(), ToolApprovalPosture::Human)],
            DaemonToolComposition::Base,
        )
        .expect_err("base composition excludes mapped families");

        assert_eq!(
            rejected,
            ConfiguredApprovalPostureError::UnknownTool { name: mapped }
        );
    }

    #[test]
    fn mapped_composition_prevalidation_accepts_a_mapped_tool() {
        let mapped = ToolName::try_new(String::from(PULL_REQUEST_METADATA_NAME))
            .expect("mapped fixture name is valid");

        DaemonToolCatalog::validate_approval_postures_for_composition(
            [(mapped, ToolApprovalPosture::Human)],
            DaemonToolComposition::WithMappedFamilies,
        )
        .expect("mapped composition includes configured families");
    }

    #[test]
    fn mapped_composition_prevalidation_accepts_a_local_git_tool() {
        let mapped = ToolName::try_new(String::from(signalbox_tools_git::GIT_STATUS_NAME))
            .expect("mapped local Git fixture name is valid");

        DaemonToolCatalog::validate_approval_postures_for_composition(
            [(mapped, ToolApprovalPosture::Human)],
            DaemonToolComposition::WithMappedFamilies,
        )
        .expect("mapped composition includes the local Git family");
    }

    #[test]
    fn composition_prevalidation_accepts_delegated_posture() {
        let echo = ToolName::try_new(String::from(ECHO_NAME)).expect("fixture name is valid");

        DaemonToolCatalog::validate_approval_postures_for_composition(
            [(echo, ToolApprovalPosture::Delegated)],
            DaemonToolComposition::Base,
        )
        .expect("the production composition wires delegated judging");
    }

    #[test]
    fn composed_catalog_applies_delegated_posture() {
        let (echo_catalog, _executor) = EchoTool::try_new()
            .expect("echo fixture compiles")
            .into_parts();
        let catalog = DaemonToolCatalog::try_new([echo_catalog])
            .expect("single-tool fixture has unique names");
        let echo = ToolName::try_new(String::from(ECHO_NAME)).expect("fixture name is valid");

        let configured = catalog
            .with_approval_postures([(echo.clone(), ToolApprovalPosture::Delegated)])
            .expect("the composed catalog accepts delegated judging");

        assert_eq!(
            configured
                .definition(&echo)
                .expect("the fixture tool remains composed")
                .approval_posture(),
            Some(ToolApprovalPosture::Delegated)
        );
    }

    #[test]
    fn pinned_workspace_filesystem_shares_one_root_after_path_replacement() {
        let parent = tempfile::tempdir().expect("fixture parent exists");
        let configured_root = parent.path().join("workspace");
        let moved_root = parent.path().join("original-workspace");
        let original_path = "original.txt";
        let replacement_path = "replacement.txt";
        let original_content = "original workspace";
        let replacement_content = "replacement workspace";
        fs::create_dir(&configured_root).expect("fixture workspace exists");
        fs::write(configured_root.join(original_path), original_content)
            .expect("fixture content is written");
        let filesystem =
            PinnedWorkspaceFileSystem::try_new(&configured_root).expect("fixture root is pinned");

        fs::rename(&configured_root, &moved_root).expect("fixture root is atomically moved");
        fs::create_dir(&configured_root).expect("replacement workspace exists");
        fs::write(configured_root.join(replacement_path), replacement_content)
            .expect("replacement content is written");

        let read_root = WorkspaceFileSystem::open_root(&filesystem, &configured_root)
            .expect("read suite receives the pinned root");
        let mutation_root = WorkspaceMutationFileSystem::open_root(&filesystem, &configured_root)
            .expect("mutation suite receives the pinned root");
        let read = WorkspaceFileSystem::read_file_prefix(
            &filesystem,
            &read_root,
            Path::new(original_path),
            original_content.len(),
        )
        .expect("read suite observes original workspace");
        let mutation_path =
            WorkspaceMutationPath::try_new(original_path).expect("fixture path is valid");
        let snapshot = WorkspaceMutationFileSystem::snapshot(
            &filesystem,
            &mutation_root,
            std::slice::from_ref(&mutation_path),
            original_content.len(),
        )
        .expect("mutation suite observes original workspace");
        let expected_snapshot_content = Some(original_content.to_owned());

        assert_eq!(read.bytes, original_content.as_bytes());
        assert_eq!(
            snapshot.content(&mutation_path),
            Some(&expected_snapshot_content)
        );
    }

    /// An absent mapping table preserves the base catalog and does not expose
    /// the families whose deployment dependencies were not injected.
    #[test]
    fn daemon_catalog_without_mappings_contains_only_base_families() {
        let web_fetch = WebFetchTool::try_new(OfflineTransport, WebFetchEgressPolicy::deny_all())
            .expect("offline web fetch tool compiles");
        let web_search = WebSearchTool::try_new(
            OfflineCredentials,
            OfflineSearchTransport,
            WebSearchConfiguration::new(WebSearchProvider::Brave),
        )
        .expect("offline web search tool compiles");
        let status =
            SessionStatusTool::try_new(OfflineWriter).expect("offline status tool compiles");
        let code_host = CodeHostTools::try_new(OfflineCredentials, OfflineCodeHostTransport)
            .expect("offline code-host tools compile");
        let (catalog, _executor) = DaemonTools::try_new_with_tools(
            || SystemTime::UNIX_EPOCH,
            ComposedToolFamilies {
                web_fetch,
                web_search,
                status,
                code_host,
                github: None::<GitHubTools<OfflineCredentials, OfflineGitHubTransport>>,
                workspace_read: None::<WorkspaceReadTools<LocalWorkspaceFileSystem>>,
                workspace_mutation: None::<WorkspaceMutationTools<LocalWorkspaceFileSystem>>,
                local_git: None::<LocalGitTools<LocalWorkspaceFileSystem>>,
                sandboxed_exec: None::<SandboxedExecTool<TokioProcessRunner>>,
                unsandboxed_exec: None::<UnsandboxedExecTool<TokioProcessRunner>>,
                cargo_diagnostics: None::<CargoDiagnosticsTool<TokioProcessRunner>>,
                conversations: None::<ConversationTools<OfflineConversationPort>>,
                plan: PlanTools::try_new(OfflineConversationPort)
                    .expect("offline plan tools compile"),
                delegation: SessionDelegationTools::try_new(
                    DaemonSessionDelegationPort::unavailable(),
                )
                .expect("offline session-delegation tools compile"),
                goal: None,
            },
        )
        .expect("base daemon tools compile")
        .into_parts();

        let definitions = catalog.definitions();
        let names = definition_names(&definitions);

        assert_eq!(
            names,
            [
                signalbox_tools_sessions::AWAIT_SESSION_NAME,
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
                signalbox_tools_plan::PLAN_READ_NAME,
                signalbox_tools_plan::PLAN_WRITE_NAME,
                REPOSITORY_LIST_DIRECTORY_NAME,
                REPOSITORY_READ_FILE_NAME,
                REVIEW_GATE_CHECK_NAME,
                signalbox_tools_sessions::SEND_SESSION_MESSAGE_NAME,
                SESSION_STATUS_UPDATE_NAME,
                signalbox_tools_sessions::SPAWN_SESSION_NAME,
                WEB_FETCH_NAME,
                WEB_SEARCH_NAME,
            ]
        );
    }

    /// Composes every injected family against offline boundaries.
    fn fully_composed_catalog(workspace: &Path) -> DaemonToolCatalog {
        DaemonTools::try_new(
            || SystemTime::UNIX_EPOCH,
            OfflineTransport,
            MappedDaemonCredentialInputs {
                web_search: OfflineCredentials,
                code_host: OfflineCredentials,
                github: OfflineCredentials,
            },
            OfflineSearchTransport,
            OfflineWriter,
            OfflineCodeHostTransport,
            OfflineGitHubTransport,
            GitHubEgressPolicy::github_api_only(),
            LocalWorkspaceFileSystem,
            workspace,
            git_identity(),
            TokioProcessRunner::try_new(
                std::env::current_exe().expect("test executable path is available"),
            )
            .expect("test executable can stand in for the unused supervisor"),
            OfflineConversationPort,
            OfflineConversationPort,
            WebFetchEgressPolicy::deny_all(),
        )
        .expect("static daemon tools compile")
        .into_parts()
        .0
    }

    /// The merged process-lifetime catalog exposes every daemon declaration in
    /// deterministic name order.
    #[test]
    fn daemon_catalog_contains_every_injected_tool_family() {
        let workspace = tempfile::tempdir().expect("workspace root exists");
        git2::Repository::init(workspace.path()).expect("fixture repository initializes");
        let catalog = fully_composed_catalog(workspace.path());

        let definitions = catalog.definitions();
        let names = definition_names(&definitions);

        assert_eq!(
            names,
            [
                APPLY_PATCH_NAME,
                signalbox_tools_sessions::AWAIT_SESSION_NAME,
                CARGO_DIAGNOSTICS_NAME,
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
                signalbox_tools_git::GIT_BRANCH_CREATE_NAME,
                signalbox_tools_git::GIT_BRANCH_SWITCH_NAME,
                signalbox_tools_git::GIT_CREATE_COMMIT_NAME,
                signalbox_tools_git::GIT_DIFF_NAME,
                signalbox_tools_git::GIT_LOG_NAME,
                signalbox_tools_git::GIT_STAGE_NAME,
                signalbox_tools_git::GIT_STATUS_NAME,
                PULL_REQUEST_DIFF_NAME,
                PULL_REQUEST_METADATA_NAME,
                PULL_REQUEST_PUBLISH_REVIEW_NAME,
                PULL_REQUEST_REVIEW_THREADS_NAME,
                GLOB_FILES_NAME,
                signalbox_tools_conversations::LIST_CONVERSATIONS_NAME,
                LIST_DIRECTORY_NAME,
                signalbox_tools_plan::PLAN_READ_NAME,
                signalbox_tools_plan::PLAN_WRITE_NAME,
                signalbox_tools_conversations::READ_CONVERSATION_NAME,
                READ_FILE_NAME,
                signalbox_tools_conversations::READ_IMPORTED_CONVERSATION_NAME,
                signalbox_tools_conversations::READ_OWN_CONVERSATION_NAME,
                REPOSITORY_LIST_DIRECTORY_NAME,
                REPOSITORY_READ_FILE_NAME,
                REVIEW_GATE_CHECK_NAME,
                SANDBOXED_EXEC_NAME,
                SEARCH_FILES_NAME,
                signalbox_tools_sessions::SEND_SESSION_MESSAGE_NAME,
                SESSION_STATUS_UPDATE_NAME,
                signalbox_tools_sessions::SPAWN_SESSION_NAME,
                UNSANDBOXED_EXEC_NAME,
                WEB_FETCH_NAME,
                WEB_SEARCH_NAME,
                WRITE_FILE_NAME,
            ]
        );
        assert_eq!(
            daemon_exec_route(SANDBOXED_EXEC_NAME),
            Some(DaemonExecRoute::Sandboxed)
        );
        assert_eq!(
            daemon_exec_route(UNSANDBOXED_EXEC_NAME),
            Some(DaemonExecRoute::Unsandboxed)
        );
        assert_eq!(
            daemon_exec_route(CARGO_DIAGNOSTICS_NAME),
            Some(DaemonExecRoute::CargoDiagnostics)
        );
    }

    const MCP_PROTOCOL_VERSION: &str = "2025-11-25";
    const MCP_UNSUPPORTED_PROTOCOL_VERSION: &str = "1900-01-01";
    const MCP_CLIENT_NAME: &str = "signalboxd-mcp-conformance";
    const MCP_INITIALIZE_REQUEST_ID: u64 = 1;
    const MCP_LIST_TOOLS_REQUEST_ID: u64 = 2;
    const MCP_CALL_WRITE_FILE_REQUEST_ID: u64 = 3;
    const MCP_SYNCHRONIZATION_REQUEST_ID: u64 = 4;
    const MCP_UNDECLARED_TOOL_REQUEST_ID: u64 = 5;
    const MCP_NON_OBJECT_ARGUMENTS_REQUEST_ID: u64 = 6;
    const MCP_ENVELOPE_REQUEST_ID: u64 = 7;
    const SYNTHETIC_JSON_RPC_ERROR_CODE: i64 = -32603;
    const SYNTHETIC_JSON_RPC_ERROR_MESSAGE: &str = "synthetic error";
    const MCP_OTHER_REQUEST_ID: u64 = 8;
    const MCP_BLOCKING_LIST_REQUEST_ID: u64 = 9;
    const MCP_UNDECLARED_TOOL_NAME: &str = "synthetic_undeclared_tool";
    const MCP_CATALOG_FILENAME: &str = "tools.json";
    const MCP_READY_FILENAME: &str = "ready";
    const MCP_PROPOSAL_PATH: &str = "bridge-must-not-write.txt";
    const MCP_PROPOSAL_CONTENT: &str = "proposal only\n";
    #[cfg(target_os = "linux")]
    struct McpBridgeFixture {
        workspace: tempfile::TempDir,
        _support: tempfile::TempDir,
        expected_tools: Vec<ComparableBridgeTool>,
        ready_path: PathBuf,
        executable: PathBuf,
        bridge: Option<McpBridgeProcess>,
    }

    #[cfg(target_os = "linux")]
    impl McpBridgeFixture {
        #[track_caller]
        fn start() -> Self {
            let workspace = tempfile::tempdir().expect("workspace root exists");
            let catalog = mapped_daemon_catalog(workspace.path());
            let definitions = catalog.definitions();
            Self::start_with_workspace_and_definitions(workspace, &definitions)
        }

        #[track_caller]
        fn start_with_workspace_and_definitions(
            workspace: tempfile::TempDir,
            definitions: &[ToolDefinition],
        ) -> Self {
            let projected_catalog = bridge_catalog(definitions);
            let expected_tools = expected_bridge_tools(definitions);
            let support = tempfile::tempdir().expect("bridge support directory exists");
            let catalog_path = support.path().join(MCP_CATALOG_FILENAME);
            let ready_path = support.path().join(MCP_READY_FILENAME);
            fs::write(&catalog_path, &projected_catalog.catalog)
                .expect("bridge catalog is written");
            let executable = projected_catalog.executable;
            let bridge = McpBridgeProcess::spawn(McpBridgeSpawn {
                executable: &executable,
                catalog: &catalog_path,
                ready: &ready_path,
                workspace: workspace.path(),
            });
            Self {
                workspace,
                _support: support,
                expected_tools,
                ready_path,
                executable,
                bridge: Some(bridge),
            }
        }

        #[track_caller]
        fn initialize(&mut self) -> serde_json::Value {
            let initialized = self.request_initialize(MCP_PROTOCOL_VERSION);
            self.bridge
                .as_mut()
                .expect("bridge remains active")
                .notify(&serde_json::json!({
                    "jsonrpc": MCP_JSON_RPC_VERSION,
                    "method": "notifications/initialized",
                }));
            initialized
        }

        #[track_caller]
        fn request_initialize(&mut self, protocol_version: &str) -> serde_json::Value {
            self.bridge
                .as_mut()
                .expect("bridge remains active")
                .request(&serde_json::json!({
                    "jsonrpc": MCP_JSON_RPC_VERSION,
                    "id": MCP_INITIALIZE_REQUEST_ID,
                    "method": "initialize",
                    "params": {
                        "protocolVersion": protocol_version,
                        "capabilities": {},
                        "clientInfo": {
                            "name": MCP_CLIENT_NAME,
                            "version": env!("CARGO_PKG_VERSION"),
                        },
                    },
                }))
        }

        #[track_caller]
        fn list_tools(&mut self) -> Vec<ComparableBridgeTool> {
            let response = self
                .bridge
                .as_mut()
                .expect("bridge remains active")
                .raw_response(&serde_json::json!({
                    "jsonrpc": MCP_JSON_RPC_VERSION,
                    "id": MCP_LIST_TOOLS_REQUEST_ID,
                    "method": "tools/list",
                    "params": {},
                }));
            let response: ListedBridgeResponse = serde_json::from_str(&response)
                .expect("MCP list response preserves raw tool schemas");
            response
                .into_tools(MCP_LIST_TOOLS_REQUEST_ID)
                .expect("MCP list response is an exclusive matching result")
        }

        #[track_caller]
        fn synchronize_without_listing(&mut self) {
            self.bridge
                .as_mut()
                .expect("bridge remains active")
                .request(&serde_json::json!({
                    "jsonrpc": MCP_JSON_RPC_VERSION,
                    "id": MCP_SYNCHRONIZATION_REQUEST_ID,
                    "method": "ping",
                    "params": {},
                }));
        }

        #[track_caller]
        fn call_write_file(&mut self) -> serde_json::Value {
            self.bridge
                .as_mut()
                .expect("bridge remains active")
                .request(&serde_json::json!({
                    "jsonrpc": MCP_JSON_RPC_VERSION,
                    "id": MCP_CALL_WRITE_FILE_REQUEST_ID,
                    "method": "tools/call",
                    "params": {
                        "name": WRITE_FILE_NAME,
                        "arguments": {
                            "path": MCP_PROPOSAL_PATH,
                            "content": MCP_PROPOSAL_CONTENT,
                        },
                    },
                }))
        }

        #[track_caller]
        fn call_undeclared_tool(&mut self) -> serde_json::Value {
            self.bridge
                .as_mut()
                .expect("bridge remains active")
                .request(&serde_json::json!({
                    "jsonrpc": MCP_JSON_RPC_VERSION,
                    "id": MCP_UNDECLARED_TOOL_REQUEST_ID,
                    "method": "tools/call",
                    "params": {
                        "name": MCP_UNDECLARED_TOOL_NAME,
                        "arguments": {},
                    },
                }))
        }

        #[track_caller]
        fn call_write_file_with_non_object_arguments(&mut self) -> serde_json::Value {
            self.bridge
                .as_mut()
                .expect("bridge remains active")
                .request(&serde_json::json!({
                    "jsonrpc": MCP_JSON_RPC_VERSION,
                    "id": MCP_NON_OBJECT_ARGUMENTS_REQUEST_ID,
                    "method": "tools/call",
                    "params": {
                        "name": WRITE_FILE_NAME,
                        "arguments": null,
                    },
                }))
        }

        #[track_caller]
        fn finish(&mut self) {
            self.bridge.take().expect("bridge remains active").finish();
        }
    }

    #[cfg(target_os = "linux")]
    #[track_caller]
    fn assert_mcp_invalid_params_response(response: &serde_json::Value, expected_id: u64) {
        let envelope = response
            .as_object()
            .expect("the MCP rejection is a JSON-RPC object");
        let error = response["error"]
            .as_object()
            .expect("the MCP rejection carries an error object");

        assert_eq!(envelope.len(), 3);
        assert_eq!(response["jsonrpc"], MCP_JSON_RPC_VERSION);
        assert_eq!(response["id"], expected_id);
        assert_eq!(response.get("result"), None);
        assert_eq!(
            error.get("code"),
            Some(&serde_json::json!(MCP_INVALID_PARAMS_ERROR_CODE))
        );
        assert!(
            error
                .get("message")
                .is_some_and(serde_json::Value::is_string)
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn claude_mcp_bridge_negotiates_the_supported_protocol() {
        let mut fixture = McpBridgeFixture::start();
        let mut initialized = fixture.initialize();
        fixture.finish();

        let server_version = initialized["result"]["serverInfo"]
            .as_object_mut()
            .expect("MCP server info is an object")
            .remove("version")
            .expect("MCP server info declares its version");
        let tools_capability = initialized["result"]["capabilities"]["tools"]
            .as_object()
            .expect("MCP tools capability is an object");

        assert_eq!(server_version, env!("CARGO_PKG_VERSION"));
        assert!(
            tools_capability
                .get("listChanged")
                .is_none_or(|value| value == &serde_json::json!(false))
        );

        initialized["result"]
            .as_object_mut()
            .expect("MCP initialization result is an object")
            .remove("capabilities")
            .expect("MCP initialization advertises capabilities");

        expect![[r#"
            {
              "protocolVersion": "2025-11-25",
              "serverInfo": {
                "name": "signalbox-claude-cli-bridge"
              }
            }"#]]
        .assert_eq(
            &serde_json::to_string_pretty(&initialized["result"])
                .expect("initialization response renders as JSON"),
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn claude_mcp_bridge_rejects_an_unsupported_protocol_version() {
        let mut fixture = McpBridgeFixture::start();
        let rejected = fixture.request_initialize(MCP_UNSUPPORTED_PROTOCOL_VERSION);
        fixture.finish();

        assert!(rejected["error"].is_object());
        assert_eq!(
            rejected["error"]["code"],
            serde_json::json!(MCP_INVALID_PARAMS_ERROR_CODE)
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn claude_mcp_bridge_lists_the_exact_daemon_catalog() {
        let mut fixture = McpBridgeFixture::start();
        fixture.initialize();
        let mut expected = fixture.expected_tools.clone();
        let mut listed = fixture.list_tools();
        fixture.finish();

        expected.sort_by(|left, right| left.name.cmp(&right.name));
        listed.sort_by(|left, right| left.name.cmp(&right.name));

        assert_eq!(listed, expected);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn claude_mcp_bridge_publishes_readiness_only_after_listing_tools() {
        let mut fixture = McpBridgeFixture::start();
        assert!(!fixture.ready_path.exists());
        fixture.initialize();
        fixture.synchronize_without_listing();
        assert!(!fixture.ready_path.exists());
        let mut waiter = McpBridgeReadyWaiter::start(McpBridgeReadyWaiterSpawn {
            executable: &fixture.executable,
            ready: &fixture.ready_path,
            workspace: fixture.workspace.path(),
        });
        waiter.synchronize_wait_path();
        assert!(!fixture.ready_path.exists());
        waiter.assert_blocks_before_listing();
        fixture.list_tools();
        waiter.finish_success();
        assert!(fixture.ready_path.is_file());
        fixture.finish();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn claude_mcp_bridge_writes_the_list_response_before_publishing_readiness() {
        let mut fixture = BlockingListResponseFixture::start();
        fixture.await_list_response_started();
        assert!(!fixture.ready_path.exists());
        let mut waiter = McpBridgeReadyWaiter::start(McpBridgeReadyWaiterSpawn {
            executable: &fixture.executable,
            ready: &fixture.ready_path,
            workspace: fixture._support.path(),
        });
        waiter.synchronize_wait_path();
        waiter.assert_blocks_while_list_response_is_backpressured();
        assert!(!fixture.ready_path.exists());
        fixture.read_list_response();
        waiter.finish_success();
        assert!(fixture.ready_path.is_file());
        fixture.finish();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn claude_mcp_bridge_acknowledges_a_workspace_proposal() {
        let mut fixture = McpBridgeFixture::start();
        fixture.initialize();
        fixture.list_tools();
        let called = fixture.call_write_file();
        fixture.finish();

        expect![[r#"{"content":[{"text":"Signalbox recorded this tool proposal for external execution.","type":"text"}],"isError":false}"#]]
            .assert_eq(&called["result"].to_string());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn claude_mcp_bridge_does_not_execute_a_workspace_proposal() {
        let mut fixture = McpBridgeFixture::start();
        fixture.initialize();
        fixture.list_tools();
        fixture.call_write_file();
        let target = fixture.workspace.path().join(MCP_PROPOSAL_PATH);
        fixture.finish();

        assert!(!target.exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn claude_mcp_bridge_rejects_an_undeclared_tool_call() {
        let mut fixture = McpBridgeFixture::start();
        fixture.initialize();
        fixture.list_tools();
        let called = fixture.call_undeclared_tool();
        fixture.finish();

        assert_mcp_invalid_params_response(&called, MCP_UNDECLARED_TOOL_REQUEST_ID);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn claude_mcp_bridge_rejects_non_object_arguments_for_a_declared_tool() {
        let mut fixture = McpBridgeFixture::start();
        fixture.initialize();
        fixture.list_tools();
        let called = fixture.call_write_file_with_non_object_arguments();
        fixture.finish();

        assert_mcp_invalid_params_response(&called, MCP_NON_OBJECT_ARGUMENTS_REQUEST_ID);
    }
    /// Root-level JSON Schema keywords an advertised argument schema may
    /// carry: object declaration, its members, and annotations.
    ///
    /// An allowlist rather than a list of known-bad combinators. A root
    /// applicator leaves the argument shape to its branches, so a validator
    /// requiring an object-typed root cannot accept it even beside a sibling
    /// `"type"` — but `oneOf`, `anyOf`, `allOf`, `not`, and `$ref` are not the
    /// closed set of those. JSON Schema also applies at the root through
    /// `if`/`then`/`else`, `dependentSchemas`, and `unevaluatedProperties`,
    /// and later drafts may add more. A blocklist naming today's rejects
    /// therefore admits tomorrow's in silence, which is the exact failure this
    /// gate exists to prevent: a root shape nobody enumerated reached a
    /// provider once already and returned 400 for every exchange offering the
    /// catalog.
    ///
    /// So the gate fails closed. A declaration needing a keyword absent here
    /// fails this test and joins the list deliberately, with the wire question
    /// answered once rather than assumed.
    const PERMITTED_ROOT_KEYWORDS: [&str; 7] = [
        "$defs",
        "additionalProperties",
        "description",
        "properties",
        "required",
        "title",
        "type",
    ];

    /// Fails one advertised schema that a function-tool wire would reject.
    ///
    /// OpenAI Chat Completions documents `tools[].function.parameters` as
    /// "The parameters the functions accepts, described as a JSON Schema
    /// object" (platform.openai.com/docs/api-reference/chat/create), and its
    /// Structured Outputs guide states the matching root rule directly: a
    /// schema root must be an `object` and must not be a root `anyOf`
    /// (platform.openai.com/docs/guides/structured-outputs). Its supported
    /// composition keyword is `anyOf` alone; `oneOf`, `allOf`, and `not`
    /// appear nowhere in that subset.
    ///
    /// This assertion pins the strictest reading of those two rules — a
    /// declared `"type": "object"` root carrying nothing outside
    /// [`PERMITTED_ROOT_KEYWORDS`]. The rejection this test exists to prevent
    /// was a root `oneOf`, and the root is what both rules constrain directly.
    ///
    /// It claims nothing past the root. Strict Structured Outputs demands more
    /// of a schema than this gate reads — every property named in `required`,
    /// `additionalProperties: false` throughout — and this catalog does not
    /// meet that: `current_time` advertises an optional `timezone`, declared
    /// but unrequired. Enabling a strict function contract would need the
    /// schema transformation the OpenAI adapter already notes, which this gate
    /// neither performs nor approximates. Passing here is evidence about the
    /// root and nothing else.
    ///
    /// Accepted cost: a schema may not express a root-level union, and must
    /// instead discriminate through one tag property, as
    /// `signalbox_tool_contract::rendered_contract_schema` now renders
    /// internally tagged argument enums.
    ///
    /// Nested combinators are untouched: only the root is constrained, so a
    /// property may still carry `oneOf`, `anyOf`, or a `$ref` into `$defs`.
    ///
    /// The stake is the blast radius, not one tool. Every request carries the
    /// whole catalog, so one rejected schema returns 400 for every exchange
    /// that offers it — not merely for calls to the offending tool.
    #[track_caller]
    fn assert_object_rooted(name: &str, schema: &str) {
        let decoded: serde_json::Value = serde_json::from_str(schema)
            .unwrap_or_else(|error| panic!("{name} schema is JSON: {error}"));
        let root = decoded
            .as_object()
            .unwrap_or_else(|| panic!("{name} schema root is a JSON object"));
        assert_eq!(
            root.get("type").and_then(serde_json::Value::as_str),
            Some("object"),
            "{name} schema root must declare \"type\": \"object\""
        );
        let unsupported = unsupported_root_keywords(root);
        assert!(
            unsupported.is_empty(),
            "{name} schema root must carry no keyword outside the advertised \
             object contract, found {unsupported:?}"
        );
    }

    /// Names the keywords a schema root declares outside the object contract.
    ///
    /// Split from the assertion so the allowlist can be exercised directly:
    /// what the gate rejects is the claim under review, and reading it back
    /// through a caught panic would prove less about which keywords it names.
    fn unsupported_root_keywords(root: &serde_json::Map<String, serde_json::Value>) -> Vec<&str> {
        root.keys()
            .map(String::as_str)
            .filter(|keyword| !PERMITTED_ROOT_KEYWORDS.contains(keyword))
            .collect()
    }

    /// Fails the first advertised declaration whose schema root a function-tool
    /// wire would reject, naming it.
    ///
    /// The iteration lives here rather than in the test body. The sweep has to
    /// cover the whole composed catalog — that is what makes a tool family
    /// added later join without anyone remembering — while a test body stays
    /// straight-line, so the loop sits behind a `#[track_caller]` helper that
    /// names the failing declaration at the call site.
    #[track_caller]
    fn assert_every_definition_is_object_rooted(definitions: &[ToolDefinition]) {
        for definition in definitions {
            assert_object_rooted(
                definition.name().as_str(),
                definition.input_schema().as_str(),
            );
        }
    }

    /// INV-055: every schema the daemon advertises satisfies the function-tool
    /// wire's root constraint, so no single declaration can fail whole
    /// exchanges.
    ///
    /// The sweep runs over the fully composed catalog rather than a listed
    /// subset: a tool family added later joins it without being remembered.
    #[test]
    fn every_advertised_tool_schema_is_object_rooted() {
        let workspace = tempfile::tempdir().expect("workspace root exists");
        git2::Repository::init(workspace.path()).expect("fixture repository initializes");
        let catalog = fully_composed_catalog(workspace.path());

        let definitions = catalog.definitions();

        assert!(!definitions.is_empty(), "the composed catalog is not empty");
        assert_every_definition_is_object_rooted(&definitions);
        // `goal_declare` compiles only against a live pool, so the composed
        // catalog cannot carry it and its static declaration joins directly.
        assert_object_rooted(GOAL_DECLARE_NAME, crate::goal_mode::GOAL_DECLARE_SCHEMA);
    }

    /// The root gate names a conditional applicator, not merely the five
    /// combinators the observed rejection happened to involve.
    ///
    /// `if`/`then`/`else` applies at the root exactly as `oneOf` does: it
    /// makes the admitted argument shape depend on a branch, which is what a
    /// function-tool root may not do. A gate written as a blocklist from one
    /// observed 400 would pass this schema through to the provider and
    /// recreate the whole-catalog rejection, so the allowlist is what is
    /// pinned here.
    #[test]
    fn the_root_gate_names_a_conditional_applicator_as_unsupported() {
        let declared = serde_json::json!({
            "if": {"required": ["base"]},
            "properties": {},
            "then": {"required": ["head"]},
            "type": "object"
        });

        assert_eq!(
            unsupported_root_keywords(declared.as_object().expect("the fixture root is an object")),
            vec!["if", "then"]
        );
    }

    /// `git_diff` is the declaration whose root `oneOf` failed every Git
    /// exchange through the OpenAI adapter. Its rendered shape is pinned so
    /// the tagged-enum root cannot come back unnoticed.
    #[test]
    fn git_diff_advertises_one_object_with_a_discriminating_scope() {
        let workspace = tempfile::tempdir().expect("workspace root exists");
        git2::Repository::init(workspace.path()).expect("fixture repository initializes");
        let catalog = fully_composed_catalog(workspace.path());
        let name = ToolName::try_new(String::from(signalbox_tools_git::GIT_DIFF_NAME))
            .expect("git_diff is a valid tool name");

        let schema: serde_json::Value = serde_json::from_str(
            catalog
                .definition(&name)
                .expect("git_diff is composed")
                .input_schema()
                .as_str(),
        )
        .expect("git_diff schema is JSON");

        assert_eq!(
            schema,
            serde_json::json!({
                "additionalProperties": false,
                "properties": {
                    "base": {
                        "description": "Older `HEAD`, full `refs/...` name, or full object ID.",
                        "maxLength": 1030,
                        "minLength": 1,
                        "type": "string"
                    },
                    "head": {
                        "description": "Newer `HEAD`, full `refs/...` name, or full object ID.",
                        "maxLength": 1030,
                        "minLength": 1,
                        "type": "string"
                    },
                    "scope": {
                        "description": "`worktree`: Includes both staged and unstaged worktree changes against HEAD. Takes no other property. `revisions`: Compares trees named by exact revision identifiers. Requires `base`, `head`.",
                        "enum": ["worktree", "revisions"],
                        "type": "string"
                    }
                },
                "required": ["scope"],
                "type": "object"
            })
        );
    }

    /// Composition preserves each execution declaration's permission default:
    /// the sandboxed command takes `Confirm`, because it accepts an arbitrary
    /// program — a compiled default an explicit posture or a session blanket
    /// can still lower, so this pins the declaration and not the resolved
    /// approval; the diagnostics
    /// reader stays automatic, since its arguments select no program and it
    /// issues only the fixed Cargo passes it builds itself — which do still run
    /// the workspace's own build scripts, macros, and test binaries; and the
    /// unsandboxed command keeps
    /// `AlwaysConfirm` — human-only regardless of the dangerous session blanket.
    /// Only an ignored live smoke observed this before, so a silent downgrade in
    /// the mapped composition could reach main unnoticed.
    #[test]
    fn composed_execution_tools_keep_their_declared_permission_defaults() {
        let workspace = tempfile::tempdir().expect("workspace root exists");
        git2::Repository::init(workspace.path()).expect("fixture repository initializes");
        let (catalog, _executor) = DaemonTools::try_new(
            || SystemTime::UNIX_EPOCH,
            OfflineTransport,
            MappedDaemonCredentialInputs {
                web_search: OfflineCredentials,
                code_host: OfflineCredentials,
                github: OfflineCredentials,
            },
            OfflineSearchTransport,
            OfflineWriter,
            OfflineCodeHostTransport,
            OfflineGitHubTransport,
            GitHubEgressPolicy::github_api_only(),
            LocalWorkspaceFileSystem,
            workspace.path(),
            git_identity(),
            TokioProcessRunner::try_new(
                std::env::current_exe().expect("test executable path is available"),
            )
            .expect("test executable can stand in for the unused supervisor"),
            OfflineConversationPort,
            OfflineConversationPort,
            WebFetchEgressPolicy::deny_all(),
        )
        .expect("static daemon tools compile")
        .into_parts();

        let permission_default = |name: &str| {
            let name = ToolName::try_new(String::from(name)).expect("fixture name is valid");
            catalog
                .definition(&name)
                .expect("the execution tool remains composed")
                .permission_default()
        };

        assert_eq!(
            permission_default(SANDBOXED_EXEC_NAME),
            ToolPermissionDefault::Confirm
        );
        assert_eq!(
            permission_default(CARGO_DIAGNOSTICS_NAME),
            ToolPermissionDefault::Auto
        );
        assert_eq!(
            permission_default(UNSANDBOXED_EXEC_NAME),
            ToolPermissionDefault::AlwaysConfirm
        );
    }
}
