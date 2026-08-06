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
    use std::{
        fmt, fs,
        io::{BufRead, BufReader, Write},
        path::{Path, PathBuf},
        process::{Child, ChildStdin, ChildStdout, Command, Stdio},
        time::SystemTime,
    };

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

    fn claude_mcp_bridge_executable() -> PathBuf {
        let current = std::env::current_exe().expect("test executable path is available");
        let profile = current
            .parent()
            .and_then(Path::parent)
            .expect("test executable is under the Cargo profile directory");
        profile.join(format!(
            "signalbox-claude-mcp-bridge{}",
            std::env::consts::EXE_SUFFIX
        ))
    }

    struct McpBridgeProcess {
        child: Child,
        input: Option<ChildStdin>,
        output: BufReader<ChildStdout>,
    }

    impl McpBridgeProcess {
        fn spawn(catalog: &Path, ready: &Path) -> Self {
            let mut child = Command::new(claude_mcp_bridge_executable())
                .arg("--serve")
                .arg(catalog)
                .arg(ready)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .expect("Claude MCP bridge binary starts");
            let input = child.stdin.take().expect("bridge stdin is piped");
            let output = BufReader::new(child.stdout.take().expect("bridge stdout is piped"));
            Self {
                child,
                input: Some(input),
                output,
            }
        }

        fn request(&mut self, request: &serde_json::Value) -> serde_json::Value {
            let input = self.input.as_mut().expect("bridge stdin remains open");
            serde_json::to_writer(&mut *input, request).expect("MCP request serializes");
            input.write_all(b"\n").expect("MCP request is written");
            input.flush().expect("MCP request is flushed");
            let mut response = String::new();
            self.output
                .read_line(&mut response)
                .expect("MCP response is read");
            serde_json::from_str(&response).expect("MCP response is JSON")
        }

        fn notify(&mut self, notification: &serde_json::Value) {
            let input = self.input.as_mut().expect("bridge stdin remains open");
            serde_json::to_writer(&mut *input, notification).expect("MCP notification serializes");
            input.write_all(b"\n").expect("MCP notification is written");
            input.flush().expect("MCP notification is flushed");
        }

        fn finish(mut self) {
            drop(self.input.take());
            let status = self.child.wait().expect("bridge process is reaped");
            assert!(status.success());
        }
    }

    impl Drop for McpBridgeProcess {
        fn drop(&mut self) {
            if self.child.try_wait().ok().flatten().is_none() {
                let _ = self.child.kill();
                let _ = self.child.wait();
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
            signalbox_domain::ToolPermissionDefault::Confirm
        );
        assert_eq!(
            web_search_definition.permission_default(),
            signalbox_domain::ToolPermissionDefault::Confirm
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

    /// The adapter-owned stdio bridge exposes the exact daemon registry and
    /// acknowledges a workspace proposal without executing it itself.
    #[test]
    fn claude_mcp_bridge_conforms_to_the_daemon_catalog() {
        let workspace = tempfile::tempdir().expect("workspace root exists");
        let catalog = mapped_daemon_catalog(workspace.path());
        let bridge_catalog = bridge_catalog(&catalog.definitions());
        let support = tempfile::tempdir().expect("bridge support directory exists");
        let catalog_path = support.path().join("tools.json");
        let ready_path = support.path().join("ready");
        fs::write(
            &catalog_path,
            serde_json::to_vec(&bridge_catalog).expect("bridge catalog serializes"),
        )
        .expect("bridge catalog is written");
        let mut bridge = McpBridgeProcess::spawn(&catalog_path, &ready_path);
        let protocol_version = "2025-11-25";
        let initialized = bridge.request(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"protocolVersion": protocol_version},
        }));
        bridge.notify(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
        }));
        let listed = bridge.request(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
            "params": {},
        }));
        let target = workspace.path().join("bridge-must-not-write.txt");
        let called = bridge.request(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": WRITE_FILE_NAME,
                "arguments": {
                    "path": "bridge-must-not-write.txt",
                    "content": "proposal only\n",
                },
            },
        }));
        bridge.finish();

        assert_eq!(initialized["result"]["protocolVersion"], protocol_version);
        assert_eq!(listed["result"]["tools"], bridge_catalog["tools"]);
        assert!(ready_path.is_file());
        assert_eq!(called["result"]["isError"], false);
        assert_eq!(called["result"]["content"][0]["type"], "text");
        assert_eq!(
            called["result"]["content"][0]["text"],
            "Signalbox recorded this tool proposal for external execution."
        );
        assert!(!target.exists());
    }
}
