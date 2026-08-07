//! Process-lifetime compiled daemon tool catalog and executor dispatch.

use std::{collections::BTreeMap, error::Error, fmt, path::Path, sync::Arc};

use signalbox_application::{
    ClassifyOperatorFailure, CompiledToolCatalog, CorrelatedToolExecutorEvidence,
    OperatorFailureClass, ToolCatalog, ToolCatalogValidationFailure, ToolDefinition,
    ToolExecutionInvocation, ToolExecutor,
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
        let goal = goal.map(GoalDeclarationTool::into_parts);
        let mut catalogs = vec![
            current_time_catalog,
            echo_catalog,
            web_fetch_catalog,
            web_search_catalog,
            status_catalog,
            code_host_catalog,
            plan_catalog,
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
    goal: Option<GoalDeclarationExecutor>,
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
            SANDBOXED_EXEC_NAME => self
                .sandboxed_exec
                .as_mut()
                .ok_or_else(DaemonToolExecutorError::unknown_tool)?
                .execute(invocation)
                .await
                .map_err(|error| DaemonToolExecutorError::from_error(&error)),
            UNSANDBOXED_EXEC_NAME => self
                .unsandboxed_exec
                .as_mut()
                .ok_or_else(DaemonToolExecutorError::unknown_tool)?
                .execute(invocation)
                .await
                .map_err(|error| DaemonToolExecutorError::from_error(&error)),
            CARGO_DIAGNOSTICS_NAME => self
                .cargo_diagnostics
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
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::os::unix::process::CommandExt;
    use std::{
        collections::BTreeSet,
        ffi::OsString,
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

    use signalbox_application::ToolCatalog;
    use signalbox_application::ToolInputSchema;
    use signalbox_domain::{ToolEffectClass, ToolPermissionDefault};
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
            .connect_lazy("postgresql://signalbox:synthetic@127.0.0.1/signalbox")
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
                goal: Some(goal),
            },
        )
        .expect("static daemon tools compile")
        .into_parts()
        .0
    }

    fn bridge_catalog(definitions: &[ToolDefinition]) -> serde_json::Value {
        let tools = definitions
            .iter()
            .map(|definition| {
                serde_json::json!({
                    "name": definition.name().as_str(),
                    "description": definition.description(),
                    "inputSchema": serde_json::from_str::<serde_json::Value>(
                        definition.input_schema().as_str(),
                    )
                    .expect("daemon tool schema remains valid JSON"),
                })
            })
            .collect::<Vec<_>>();
        serde_json::json!({"tools": tools})
    }

    const SYNTHETIC_BRIDGE_TOOL_NAME: &str = "synthetic_bridge_tool";
    const SYNTHETIC_BRIDGE_TOOL_DESCRIPTION: &str = "Projects a synthetic bridge tool.";
    const SYNTHETIC_BRIDGE_TOOL_SCHEMA: &str =
        r#"{"properties":{"value":{"type":"string"}},"required":["value"],"type":"object"}"#;

    fn synthetic_bridge_tool_definition() -> ToolDefinition {
        ToolDefinition::new(
            ToolName::try_new(String::from(SYNTHETIC_BRIDGE_TOOL_NAME))
                .expect("synthetic bridge tool name is valid"),
            String::from(SYNTHETIC_BRIDGE_TOOL_DESCRIPTION),
            ToolInputSchema::try_new(String::from(SYNTHETIC_BRIDGE_TOOL_SCHEMA))
                .expect("synthetic bridge tool schema is valid"),
            ToolPermissionDefault::Confirm,
            ToolEffectClass::EffectFree,
        )
    }

    #[test]
    fn bridge_catalog_projects_definition_fields_into_mcp_shape() {
        let definition = synthetic_bridge_tool_definition();

        assert_eq!(
            bridge_catalog(&[definition]),
            serde_json::json!({
                "tools": [{
                    "name": SYNTHETIC_BRIDGE_TOOL_NAME,
                    "description": SYNTHETIC_BRIDGE_TOOL_DESCRIPTION,
                    "inputSchema": serde_json::from_str::<serde_json::Value>(
                        SYNTHETIC_BRIDGE_TOOL_SCHEMA,
                    )
                    .expect("synthetic expected schema is valid JSON"),
                }],
            })
        );
    }

    struct BridgeArtifactSelection {
        profile: OsString,
        target: Option<OsString>,
        target_dir: PathBuf,
    }

    const CLAUDE_MCP_BRIDGE_BINARY: &str = "signalbox-claude-mcp-bridge";
    const CARGO_TEST_PROFILE: &str = "test";

    fn claude_mcp_bridge_artifact_selection() -> BridgeArtifactSelection {
        let current = std::env::current_exe().expect("test executable path is available");
        let known_targets = rustc_target_names();
        let configured_target_dir = configured_cargo_target_dir(&current);
        let default_target_dir = cargo_metadata_target_dir();
        reject_unrecognized_default_target(
            &current,
            configured_target_dir.as_deref(),
            &default_target_dir,
            &known_targets,
        );
        claude_mcp_bridge_artifact_selection_for(
            &current,
            CARGO_TEST_PROFILE,
            configured_target_dir.as_deref(),
            &default_target_dir,
            &known_targets,
        )
    }

    fn configured_cargo_target_dir(current: &Path) -> Option<PathBuf> {
        let configured = PathBuf::from(std::env::var_os("CARGO_TARGET_DIR")?);
        let configured = configured_cargo_target_dir_for(current, &configured);
        Some(canonicalized_target_dir(&configured))
    }

    fn configured_cargo_target_dir_for(current: &Path, configured: &Path) -> PathBuf {
        if configured.is_absolute() {
            return configured.to_path_buf();
        }
        let configured = lexically_normalized(configured);
        current
            .ancestors()
            .find(|ancestor| ancestor.ends_with(&configured))
            .or_else(|| {
                let configured_name = configured.file_name()?;
                current
                    .ancestors()
                    .find(|ancestor| ancestor.file_name() == Some(configured_name))
            })
            .expect("relative Cargo target directory is an ancestor of the test executable")
            .to_path_buf()
    }

    fn canonicalized_target_dir(configured: &Path) -> PathBuf {
        fs::canonicalize(configured).expect("configured Cargo target directory canonicalizes")
    }

    fn cargo_metadata_target_dir() -> PathBuf {
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
        let output = Command::new(cargo)
            .args(["metadata", "--no-deps", "--format-version", "1"])
            .output()
            .expect("Cargo target metadata is available");
        assert!(output.status.success(), "Cargo target metadata succeeds");
        let metadata: serde_json::Value =
            serde_json::from_slice(&output.stdout).expect("Cargo target metadata is valid JSON");
        let target_dir = metadata["target_directory"]
            .as_str()
            .expect("Cargo target metadata names the artifact directory");
        lexically_normalized(Path::new(target_dir))
    }

    fn reject_unrecognized_default_target(
        current: &Path,
        configured_target_dir: Option<&Path>,
        default_target_dir: &Path,
        known_targets: &BTreeSet<OsString>,
    ) {
        if configured_target_dir.is_some() {
            return;
        }
        let current = lexically_normalized(current);
        let default_target_dir = lexically_normalized(default_target_dir);
        let artifact_parent = current
            .parent()
            .and_then(Path::parent)
            .and_then(Path::parent)
            .expect("test executable has a Cargo artifact parent");
        let artifact_parent_name = artifact_parent
            .file_name()
            .expect("Cargo artifact parent has a name");
        assert!(
            artifact_parent.parent() != Some(default_target_dir.as_path())
                || known_targets.contains(artifact_parent_name),
            "custom Cargo target specifications are unsupported by the nested bridge build"
        );
    }

    fn rustc_target_names() -> BTreeSet<OsString> {
        let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| OsString::from("rustc"));
        let output = Command::new(rustc)
            .args(["--print", "target-list"])
            .output()
            .expect("rustc target inventory is available");
        assert!(output.status.success(), "rustc target inventory succeeds");
        String::from_utf8(output.stdout)
            .expect("rustc target inventory is UTF-8")
            .lines()
            .map(OsString::from)
            .collect()
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

    fn claude_mcp_bridge_artifact_selection_for(
        current: &Path,
        debug_profile: &str,
        configured_target_dir: Option<&Path>,
        default_target_dir: &Path,
        known_targets: &BTreeSet<OsString>,
    ) -> BridgeArtifactSelection {
        let current = lexically_normalized(current);
        let configured_target_dir = configured_target_dir.map(lexically_normalized);
        let default_target_dir = lexically_normalized(default_target_dir);
        let profile_dir = current
            .parent()
            .and_then(Path::parent)
            .expect("test executable is under the Cargo profile directory");
        let profile_dir_name = profile_dir
            .file_name()
            .expect("Cargo profile directory has a name");
        let profile = match profile_dir_name.to_str() {
            Some("debug") => OsString::from(debug_profile),
            Some("release") => OsString::from("release"),
            _ => profile_dir_name.to_os_string(),
        };
        let artifact_parent = profile_dir
            .parent()
            .expect("Cargo profile has an artifact parent");
        let artifact_parent_name = artifact_parent
            .file_name()
            .expect("Cargo artifact parent has a name");
        let (target_dir, target) = if let Some(target_dir) = configured_target_dir.as_deref() {
            if artifact_parent == target_dir {
                (target_dir.to_path_buf(), None)
            } else {
                assert_eq!(
                    artifact_parent.parent(),
                    Some(target_dir),
                    "Cargo target-specific profile is directly below the configured target directory"
                );
                assert!(
                    known_targets.contains(artifact_parent_name),
                    "custom Cargo target specifications are unsupported by the nested bridge build"
                );
                (
                    target_dir.to_path_buf(),
                    Some(artifact_parent_name.to_os_string()),
                )
            }
        } else if artifact_parent.parent() == Some(default_target_dir.as_path())
            && known_targets.contains(artifact_parent_name)
        {
            (
                artifact_parent
                    .parent()
                    .expect("Cargo target-specific artifacts have a target directory")
                    .to_path_buf(),
                Some(artifact_parent_name.to_os_string()),
            )
        } else {
            (artifact_parent.to_path_buf(), None)
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
        let selection = claude_mcp_bridge_artifact_selection_for(
            expectation.executable,
            expectation.debug_profile,
            expectation.configured_target_dir,
            expectation.default_target_dir,
            &known_targets,
        );

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
        let executable = Path::new("synthetic-target/debug/deps/daemon-tools-test");

        assert_bridge_artifact_selection(BridgeArtifactExpectation {
            executable,
            target_dir: Path::new("synthetic-target"),
            configured_target_dir: None,
            default_target_dir: Path::new("synthetic-target"),
            debug_profile: CARGO_TEST_PROFILE,
            expected_profile: CARGO_TEST_PROFILE,
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
            debug_profile: CARGO_TEST_PROFILE,
            expected_profile: "release",
            expected_target: None,
            recognized_target: None,
        });
    }

    #[test]
    fn bridge_artifact_selection_preserves_a_custom_profile() {
        let executable = Path::new("synthetic-target/ci-fast/deps/daemon-tools-test");

        assert_bridge_artifact_selection(BridgeArtifactExpectation {
            executable,
            target_dir: Path::new("synthetic-target"),
            configured_target_dir: None,
            default_target_dir: Path::new("synthetic-target"),
            debug_profile: CARGO_TEST_PROFILE,
            expected_profile: "ci-fast",
            expected_target: None,
            recognized_target: None,
        });
    }

    const SYNTHETIC_CARGO_TARGET: &str = "x86_64-unknown-linux-musl";

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
    fn bridge_artifact_selection_does_not_infer_target_from_cli_target_dir_name() {
        let target_dir = Path::new("synthetic-parent").join(SYNTHETIC_CARGO_TARGET);
        let executable = target_dir.join("debug/deps/daemon-tools-test");

        assert_bridge_artifact_selection(BridgeArtifactExpectation {
            executable: &executable,
            target_dir: &target_dir,
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

        reject_unrecognized_default_target(&executable, None, target_dir, &BTreeSet::new());
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
            configured_cargo_target_dir_for(&executable, Path::new("relative-target"));

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
    fn bridge_artifact_selection_resolves_a_parent_relative_configured_directory() {
        let target_dir = Path::new("synthetic-parent/artifact");
        let executable = target_dir
            .join(SYNTHETIC_CARGO_TARGET)
            .join("debug/deps/daemon-tools-test");
        let configured_target_dir =
            configured_cargo_target_dir_for(&executable, Path::new("../artifact"));

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
    const BRIDGE_RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);
    const BRIDGE_EXIT_TIMEOUT: Duration = Duration::from_secs(12);
    const BRIDGE_CHILD_TEST_TIMEOUT: Duration = Duration::from_millis(25);
    const CHILD_POLL_INTERVAL: Duration = Duration::from_millis(10);
    #[cfg(target_os = "linux")]
    const LINUX_NANOSLEEP_WAIT_CHANNEL: &str = "nanosleep";
    static BRIDGE_BUILD_LOCK: Mutex<()> = Mutex::new(());

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

    fn terminate_owned_process_group(child: &mut Child) {
        #[cfg(unix)]
        if let Some(group) = rustix::process::Pid::from_raw(child.id() as i32) {
            let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
        }
        terminate_child(child);
    }

    #[test]
    #[ignore = "subprocess fixture for the bounded child-wait regression test"]
    fn bridge_wait_child_fixture() {
        thread::sleep(Duration::from_secs(2));
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "subprocess fixture that holds its parent's stderr open"]
    fn bridge_wait_descendant_fixture() {
        thread::sleep(Duration::from_secs(30));
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

        assert!(
            wait_for_child(&mut child, BRIDGE_CHILD_TEST_TIMEOUT)
                .expect("bounded wait observes the live child")
                .is_none()
        );
        terminate_owned_process_group(&mut child);
        assert!(
            child
                .try_wait()
                .expect("cleaned child status is readable")
                .is_some()
        );
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

    #[track_caller]
    fn ensure_claude_mcp_bridge_executable() -> PathBuf {
        let _build_guard = BRIDGE_BUILD_LOCK
            .lock()
            .expect("bridge build lock is available");
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("signalboxd manifest has a workspace root")
            .to_path_buf();
        let selection = claude_mcp_bridge_artifact_selection();
        require_direct_bridge_execution(&selection);
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
        let mut build_command = Command::new(cargo);
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
            .current_dir(workspace)
            .stdout(Stdio::piped());
        if let Some(target) = &selection.target {
            build_command.arg("--target").arg(target);
        }
        configure_owned_process_group(&mut build_command);
        let mut build = build_command.spawn().expect("bridge binary build starts");
        let (messages, reader) = response_reader(BufReader::new(
            build.stdout.take().expect("Cargo build stdout is piped"),
        ));
        let Some(status) = wait_for_child(&mut build, BRIDGE_BUILD_TIMEOUT)
            .expect("bridge binary build is observed")
        else {
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
        let executable = PathBuf::from("synthetic-target/bridge");
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

    struct FailingBridgeReader;

    impl Read for FailingBridgeReader {
        fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::new(
                SYNTHETIC_BRIDGE_READER_ERROR_KIND,
                "synthetic failure",
            ))
        }
    }

    impl BufRead for FailingBridgeReader {
        fn fill_buf(&mut self) -> io::Result<&[u8]> {
            Err(io::Error::new(
                SYNTHETIC_BRIDGE_READER_ERROR_KIND,
                "synthetic failure",
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
                .arg("--serve")
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
            let input = self.input.as_mut().expect("bridge stdin remains open");
            serde_json::to_writer(&mut *input, request).expect("MCP request serializes");
            input.write_all(b"\n").expect("MCP request is written");
            input.flush().expect("MCP request is flushed");
            let response = match self.responses.recv_timeout(BRIDGE_RESPONSE_TIMEOUT) {
                Ok(Ok(response)) => response,
                Ok(Err(error)) => panic!("MCP response read failed: {error}"),
                Err(RecvTimeoutError::Disconnected) => {
                    panic!("MCP bridge closed stdout before responding")
                }
                Err(RecvTimeoutError::Timeout) => {
                    terminate_child(&mut self.child);
                    panic!("MCP bridge response exceeded its timeout")
                }
            };
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
            == Some("2.0")
            && envelope.response.get("id") == Some(envelope.request_id)
            && has_result != has_error
    }

    #[test]
    fn mcp_response_envelope_rejects_a_wrong_protocol_version() {
        let request_id = serde_json::json!(MCP_ENVELOPE_REQUEST_ID);
        let response =
            serde_json::json!({"jsonrpc": "1.0", "id": request_id.clone(), "result": {}});

        assert!(!valid_mcp_response_envelope(McpResponseEnvelope {
            response: &response,
            request_id: &request_id,
        }));
    }

    #[test]
    fn mcp_response_envelope_rejects_a_mismatched_request_identity() {
        let request_id = serde_json::json!(MCP_ENVELOPE_REQUEST_ID);
        let response =
            serde_json::json!({"jsonrpc": "2.0", "id": MCP_OTHER_REQUEST_ID, "result": {}});

        assert!(!valid_mcp_response_envelope(McpResponseEnvelope {
            response: &response,
            request_id: &request_id,
        }));
    }

    #[test]
    fn mcp_response_envelope_rejects_result_and_error_together() {
        let request_id = serde_json::json!(MCP_ENVELOPE_REQUEST_ID);
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": request_id.clone(),
            "result": {},
            "error": {"code": -32603, "message": "synthetic error"},
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

    struct McpBridgeReadyWaiter {
        child: Child,
    }

    struct McpBridgeReadyWaiterSpawn<'a> {
        executable: &'a Path,
        ready: &'a Path,
        workspace: &'a Path,
    }

    impl McpBridgeReadyWaiter {
        fn start(config: McpBridgeReadyWaiterSpawn<'_>) -> Self {
            let child = Command::new(config.executable)
                .arg("--wait-ready")
                .arg(config.ready)
                .current_dir(config.workspace)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("bridge readiness waiter starts");
            Self { child }
        }

        #[cfg(target_os = "linux")]
        #[track_caller]
        fn await_polling(&mut self) {
            let wait_channel = Path::new("/proc")
                .join(self.child.id().to_string())
                .join("wchan");
            let deadline = Instant::now() + BRIDGE_RESPONSE_TIMEOUT;
            loop {
                assert!(
                    self.child
                        .try_wait()
                        .expect("bridge readiness waiter status is readable")
                        .is_none(),
                    "bridge readiness waiter remains active while synchronizing"
                );
                let observed = fs::read_to_string(&wait_channel)
                    .expect("bridge readiness waiter exposes its Linux wait channel");
                if observed.contains(LINUX_NANOSLEEP_WAIT_CHANNEL) {
                    return;
                }
                assert!(
                    Instant::now() < deadline,
                    "bridge readiness waiter reaches its polling sleep"
                );
                thread::sleep(CHILD_POLL_INTERVAL);
            }
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

    impl Drop for McpBridgeReadyWaiter {
        fn drop(&mut self) {
            if self.child.try_wait().ok().flatten().is_none() {
                terminate_child(&mut self.child);
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
                SESSION_STATUS_UPDATE_NAME,
                WEB_FETCH_NAME,
                WEB_SEARCH_NAME,
            ]
        );
    }

    /// The merged process-lifetime catalog exposes every daemon declaration in
    /// deterministic name order.
    #[test]
    fn daemon_catalog_contains_every_injected_tool_family() {
        let workspace = tempfile::tempdir().expect("workspace root exists");
        let catalog = mapped_daemon_catalog(workspace.path());

        let definitions = catalog.definitions();
        let names = definition_names(&definitions);

        assert_eq!(
            names,
            [
                APPLY_PATCH_NAME,
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
                GOAL_DECLARE_NAME,
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
                SESSION_STATUS_UPDATE_NAME,
                UNSANDBOXED_EXEC_NAME,
                WEB_FETCH_NAME,
                WEB_SEARCH_NAME,
                WRITE_FILE_NAME,
            ]
        );
    }

    const MCP_PROTOCOL_VERSION: &str = "2025-11-25";
    const MCP_SERVER_NAME: &str = "signalbox-claude-cli-bridge";
    const MCP_CLIENT_NAME: &str = "signalboxd-mcp-conformance";
    const MCP_INITIALIZE_REQUEST_ID: u64 = 1;
    const MCP_LIST_TOOLS_REQUEST_ID: u64 = 2;
    const MCP_CALL_WRITE_FILE_REQUEST_ID: u64 = 3;
    const MCP_SYNCHRONIZATION_REQUEST_ID: u64 = 4;
    const MCP_UNDECLARED_TOOL_REQUEST_ID: u64 = 5;
    const MCP_NON_OBJECT_ARGUMENTS_REQUEST_ID: u64 = 6;
    const MCP_ENVELOPE_REQUEST_ID: u64 = 7;
    const MCP_OTHER_REQUEST_ID: u64 = 8;
    const MCP_UNDECLARED_TOOL_NAME: &str = "synthetic_undeclared_tool";
    const MCP_PROPOSAL_PATH: &str = "bridge-must-not-write.txt";
    const MCP_PROPOSAL_CONTENT: &str = "proposal only\n";
    const MCP_PROPOSAL_ACKNOWLEDGEMENT: &str =
        "Signalbox recorded this tool proposal for external execution.";

    fn invalid_tool_call_error() -> serde_json::Value {
        serde_json::json!({
            "code": -32602,
            "message": "undeclared tool or non-object arguments",
        })
    }

    struct McpBridgeFixture {
        workspace: tempfile::TempDir,
        _support: tempfile::TempDir,
        expected_catalog: serde_json::Value,
        ready_path: PathBuf,
        executable: PathBuf,
        bridge: Option<McpBridgeProcess>,
    }

    impl McpBridgeFixture {
        #[track_caller]
        fn start() -> Self {
            let workspace = tempfile::tempdir().expect("workspace root exists");
            let catalog = mapped_daemon_catalog(workspace.path());
            let expected_catalog = bridge_catalog(&catalog.definitions());
            let support = tempfile::tempdir().expect("bridge support directory exists");
            let catalog_path = support.path().join("tools.json");
            let ready_path = support.path().join("ready");
            fs::write(
                &catalog_path,
                serde_json::to_vec(&expected_catalog).expect("bridge catalog serializes"),
            )
            .expect("bridge catalog is written");
            let executable = ensure_claude_mcp_bridge_executable();
            let bridge = McpBridgeProcess::spawn(McpBridgeSpawn {
                executable: &executable,
                catalog: &catalog_path,
                ready: &ready_path,
                workspace: workspace.path(),
            });
            Self {
                workspace,
                _support: support,
                expected_catalog,
                ready_path,
                executable,
                bridge: Some(bridge),
            }
        }

        #[track_caller]
        fn initialize(&mut self) -> serde_json::Value {
            let initialized = self
                .bridge
                .as_mut()
                .expect("bridge remains active")
                .request(&serde_json::json!({
                    "jsonrpc": "2.0",
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
                }));
            self.bridge
                .as_mut()
                .expect("bridge remains active")
                .notify(&serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/initialized",
                }));
            initialized
        }

        #[track_caller]
        fn list_tools(&mut self) -> serde_json::Value {
            self.bridge
                .as_mut()
                .expect("bridge remains active")
                .request(&serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": MCP_LIST_TOOLS_REQUEST_ID,
                    "method": "tools/list",
                    "params": {},
                }))
        }

        #[track_caller]
        fn synchronize_without_listing(&mut self) {
            self.bridge
                .as_mut()
                .expect("bridge remains active")
                .request(&serde_json::json!({
                    "jsonrpc": "2.0",
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
                    "jsonrpc": "2.0",
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
                    "jsonrpc": "2.0",
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
                    "jsonrpc": "2.0",
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

    #[test]
    fn claude_mcp_bridge_negotiates_the_supported_protocol() {
        let mut fixture = McpBridgeFixture::start();
        let initialized = fixture.initialize();
        fixture.finish();

        assert_eq!(
            initialized["result"]["protocolVersion"],
            MCP_PROTOCOL_VERSION
        );
        assert_eq!(
            initialized["result"]["capabilities"]["tools"],
            serde_json::json!({"listChanged": false})
        );
        assert_eq!(
            initialized["result"]["serverInfo"],
            serde_json::json!({
                "name": MCP_SERVER_NAME,
                "version": env!("CARGO_PKG_VERSION"),
            })
        );
    }

    #[test]
    fn claude_mcp_bridge_lists_the_exact_daemon_catalog() {
        let mut fixture = McpBridgeFixture::start();
        fixture.initialize();
        let expected = fixture.expected_catalog["tools"].clone();
        let listed = fixture.list_tools();
        fixture.finish();

        assert_eq!(listed["result"]["tools"], expected);
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
        waiter.await_polling();
        assert!(!fixture.ready_path.exists());
        fixture.list_tools();
        waiter.finish_success();
        assert!(fixture.ready_path.is_file());
        fixture.finish();
    }

    #[test]
    fn claude_mcp_bridge_acknowledges_a_workspace_proposal() {
        let mut fixture = McpBridgeFixture::start();
        fixture.initialize();
        fixture.list_tools();
        let called = fixture.call_write_file();
        fixture.finish();

        assert_eq!(called["result"]["isError"], false);
        assert_eq!(
            called["result"]["content"],
            serde_json::json!([{
                "type": "text",
                "text": MCP_PROPOSAL_ACKNOWLEDGEMENT,
            }])
        );
    }

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

    #[test]
    fn claude_mcp_bridge_rejects_an_undeclared_tool_call() {
        let mut fixture = McpBridgeFixture::start();
        fixture.initialize();
        fixture.list_tools();
        let called = fixture.call_undeclared_tool();
        fixture.finish();

        assert_eq!(called["error"], invalid_tool_call_error());
    }

    #[test]
    fn claude_mcp_bridge_rejects_non_object_arguments_for_a_declared_tool() {
        let mut fixture = McpBridgeFixture::start();
        fixture.initialize();
        fixture.list_tools();
        let called = fixture.call_write_file_with_non_object_arguments();
        fixture.finish();

        assert_eq!(called["error"], invalid_tool_call_error());
    }
}
