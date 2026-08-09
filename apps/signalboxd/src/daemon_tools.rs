//! Process-lifetime compiled daemon tool catalog and executor dispatch.
//!
//! The catalog is one process-lifetime immutable compiled value; the executors
//! a workspace root binds are per session, resolved through
//! [`SessionWorkspaceRoots`]. See `docs/spec/tool-loop.md` and
//! `docs/spec/git-authority-threat-model.md`.

use std::{
    collections::BTreeMap,
    error::Error,
    fmt,
    path::{Path, PathBuf},
    sync::Arc,
};

use signalbox_application::{
    ClassifyOperatorFailure, CompiledToolCatalog, CorrelatedDurableChildWait,
    CorrelatedToolExecutorEvidence, OperatorFailureClass, ToolCatalog,
    ToolCatalogValidationFailure, ToolDefinition, ToolExecutionInvocation, ToolExecutor,
    ToolExecutorDisposition, ToolExecutorEvidence,
};
use signalbox_domain::{
    NormalizedToolArguments, SessionId, ToolApprovalPosture, ToolExecutionErrorDetail, ToolName,
};
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
use signalbox_tools_git::{
    GitIdentity, GitObjectFormat, LOCAL_GIT_TOOL_NAMES, LocalGitExecutor, LocalGitTools,
};
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
///
/// One adapter binds exactly one root: [`WorkspaceFileSystem::open_root`] and
/// [`WorkspaceMutationFileSystem::open_root`] both ignore the path they are
/// handed and return the root this adapter pinned at construction. A second
/// root therefore requires a second adapter, never a second call.
#[derive(Clone, Debug)]
pub struct PinnedWorkspaceFileSystem {
    root: WorkspaceRoot,
    local: LocalWorkspaceFileSystem,
}

impl PinnedWorkspaceFileSystem {
    /// Opens one root exactly once for the lifetime of this adapter.
    pub fn try_new(root: &Path) -> Result<Self, WorkspaceRootError> {
        let local = LocalWorkspaceFileSystem;
        let root = WorkspaceRoot::try_new(&local, root)?;
        Ok(Self { root, local })
    }
}

/// Opens a further workspace root through one more adapter of the same kind.
///
/// Composing a second workspace-bound family needs a second adapter rather than
/// a second call, because [`PinnedWorkspaceFileSystem`] structurally cannot open
/// a root other than the one it pinned. This trait is that construction step,
/// stated once so the composition below is generic over it.
pub trait PinFurtherWorkspaceRoot: Sized {
    /// Opens one root and returns the adapter bound to it.
    fn pin_further_root(root: &Path) -> Result<Self, WorkspaceRootError>;
}

impl PinFurtherWorkspaceRoot for PinnedWorkspaceFileSystem {
    fn pin_further_root(root: &Path) -> Result<Self, WorkspaceRootError> {
        Self::try_new(root)
    }
}

impl PinFurtherWorkspaceRoot for LocalWorkspaceFileSystem {
    /// The local adapter holds no root, so every root is reachable through one
    /// value; the suites it is injected into hold the pinned root instead.
    fn pin_further_root(_root: &Path) -> Result<Self, WorkspaceRootError> {
        Ok(Self)
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

/// Directory name suffix appended to the configured workspace root's own name
/// to form the parent of every derived per-session root.
///
/// A sibling rather than a child: a per-session root nested under the
/// configured root would be readable, writable, and executable by every session
/// still bound to that configured root, which is the isolation the derivation
/// exists to establish.
const SESSION_WORKSPACE_DIRECTORY_SUFFIX: &str = ".sessions";

/// Largest number of derived per-session roots whose executors are retained.
///
/// Each retained entry holds open directory descriptors and one pinned
/// repository, so the bound is what keeps descriptor use finite; the least
/// recently used entry is dropped when a further session arrives.
const MAX_RETAINED_SESSION_WORKSPACES: usize = 8;

const SESSION_WORKSPACE_UNAVAILABLE_DETAIL: &str = "session workspace is unavailable";

/// Derives each session's workspace root from the configured root by a fixed
/// formula.
///
/// A session names no path: the derivation takes only the configured root and
/// the session's own identity, so the set of roots the daemon can ever open is
/// determined by deployment configuration alone.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionWorkspaceRoots {
    configured: PathBuf,
    derived_parent: Option<PathBuf>,
}

impl SessionWorkspaceRoots {
    /// Fixes the derivation against one configured workspace root.
    pub fn new(configured: &Path) -> Self {
        let derived_parent =
            configured
                .parent()
                .zip(configured.file_name())
                .map(|(parent, name)| {
                    let mut directory = name.to_owned();
                    directory.push(SESSION_WORKSPACE_DIRECTORY_SUFFIX);
                    parent.join(directory)
                });
        Self {
            configured: configured.to_owned(),
            derived_parent,
        }
    }

    /// Returns the path the formula assigns one session, before asking whether
    /// a directory exists there.
    ///
    /// Absent only when the configured root has no parent or no final
    /// component, which no absolute configured root outside the filesystem
    /// root itself has.
    #[must_use]
    pub fn derived_path(&self, session: SessionId) -> Option<PathBuf> {
        self.derived_parent
            .as_ref()
            .map(|parent| parent.join(session.into_uuid().to_string()))
    }

    /// Returns the root one session's workspace-bound tools bind.
    #[must_use]
    pub fn resolve(&self, session: SessionId) -> SessionWorkspaceRoot {
        match self.derived_path(session) {
            Some(path) if path.is_dir() => SessionWorkspaceRoot::Derived { path },
            Some(_) | None => SessionWorkspaceRoot::ConfiguredRoot,
        }
    }
}

/// Which root one session's workspace-bound tools bind.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionWorkspaceRoot {
    /// A directory exists at the session's derived path and binds it alone.
    Derived {
        /// The derived absolute path.
        path: PathBuf,
    },
    /// No directory exists at the session's derived path, so the session binds
    /// the configured root that every session bound before this derivation.
    ConfiguredRoot,
}

/// The six executors one workspace root binds.
struct WorkspaceBoundExecutors<FileSystem: WorkspaceMutationFileSystem, ExecRunner: ProcessRunner> {
    workspace_read: WorkspaceReadExecutor<FileSystem>,
    workspace_mutation: SharedToolExecutor<WorkspaceMutationExecutor<FileSystem>>,
    local_git: SharedToolExecutor<LocalGitExecutor<FileSystem>>,
    sandboxed_exec: ExecExecutor<SandboxedCommandRunner<ExecRunner>>,
    unsandboxed_exec: ExecExecutor<UnsandboxedCommandRunner<ExecRunner>>,
    cargo_diagnostics: CargoDiagnosticsExecutor<ExecRunner>,
    git_object_format: GitObjectFormat,
}

impl<FileSystem: WorkspaceMutationFileSystem, ExecRunner: ProcessRunner> Clone
    for WorkspaceBoundExecutors<FileSystem, ExecRunner>
{
    fn clone(&self) -> Self {
        Self {
            workspace_read: self.workspace_read.clone(),
            workspace_mutation: self.workspace_mutation.clone(),
            local_git: self.local_git.clone(),
            sandboxed_exec: self.sandboxed_exec.clone(),
            unsandboxed_exec: self.unsandboxed_exec.clone(),
            cargo_diagnostics: self.cargo_diagnostics.clone(),
            git_object_format: self.git_object_format,
        }
    }
}

impl<FileSystem: WorkspaceMutationFileSystem, ExecRunner: ProcessRunner> fmt::Debug
    for WorkspaceBoundExecutors<FileSystem, ExecRunner>
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkspaceBoundExecutors")
            .finish_non_exhaustive()
    }
}

/// One root's compiled declarations beside the executors bound to it.
struct WorkspaceBoundFamilies<FileSystem: WorkspaceMutationFileSystem, ExecRunner: ProcessRunner> {
    catalogs: [CompiledToolCatalog; 6],
    executors: WorkspaceBoundExecutors<FileSystem, ExecRunner>,
}

impl<FileSystem, ExecRunner> WorkspaceBoundFamilies<FileSystem, ExecRunner>
where
    FileSystem: WorkspaceFileSystem + WorkspaceMutationFileSystem,
    ExecRunner: ProcessRunner,
{
    /// Composes every workspace-root-bound family around one root.
    ///
    /// The root stays construction input for each family exactly as before:
    /// the filesystem adapter is already bound to it, the execution suites
    /// capture its identity, and the Git suite validates its repository layout.
    fn try_new(
        filesystem: FileSystem,
        root: &Path,
        git_identity: GitIdentity,
        exec_runner: ExecRunner,
    ) -> Result<Self, DaemonToolsConstructionError> {
        let workspace_read = WorkspaceReadTools::try_new(filesystem.clone(), root)
            .map_err(|_| DaemonToolsConstructionError::WorkspaceRead)?;
        let workspace_mutation = WorkspaceMutationTools::try_new(filesystem.clone(), root)
            .map_err(|_| DaemonToolsConstructionError::WorkspaceMutation)?;
        let local_git = LocalGitTools::try_new(filesystem, root, git_identity)
            .map_err(|_| DaemonToolsConstructionError::LocalGit)?;
        let git_object_format = local_git.object_format();
        let sandboxed_exec = SandboxedExecTool::try_new(exec_runner.clone(), root)
            .map_err(|_| DaemonToolsConstructionError::Exec)?;
        let unsandboxed_exec = UnsandboxedExecTool::try_new(exec_runner.clone(), root)
            .map_err(|_| DaemonToolsConstructionError::Exec)?;
        let cargo_diagnostics = CargoDiagnosticsTool::try_new(exec_runner, root)
            .map_err(|_| DaemonToolsConstructionError::Exec)?;
        let (workspace_read_catalog, workspace_read) = workspace_read.into_parts();
        let (workspace_mutation_catalog, workspace_mutation) = workspace_mutation.into_parts();
        let (local_git_catalog, local_git) = local_git.into_parts();
        let (sandboxed_exec_catalog, sandboxed_exec) = sandboxed_exec.into_parts();
        let (unsandboxed_exec_catalog, unsandboxed_exec) = unsandboxed_exec.into_parts();
        let (cargo_diagnostics_catalog, cargo_diagnostics) = cargo_diagnostics.into_parts();
        Ok(Self {
            catalogs: [
                workspace_read_catalog,
                workspace_mutation_catalog,
                local_git_catalog,
                sandboxed_exec_catalog,
                unsandboxed_exec_catalog,
                cargo_diagnostics_catalog,
            ],
            executors: WorkspaceBoundExecutors {
                workspace_read,
                workspace_mutation: SharedToolExecutor::new(workspace_mutation),
                local_git: SharedToolExecutor::new(local_git),
                sandboxed_exec,
                unsandboxed_exec,
                cargo_diagnostics,
                git_object_format,
            },
        })
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
    workspace_bound: Option<ConfiguredWorkspaceComposition<FileSystem, ExecRunner>>,
    conversations: Option<ConversationTools<ConversationPort>>,
    plan: PlanTools<PlanPort>,
    delegation: SessionDelegationTools<DaemonSessionDelegationPort>,
    goal: Option<GoalDeclarationTool>,
}

/// The configured root's own families beside the derivation later sessions use.
struct ConfiguredWorkspaceComposition<
    FileSystem: WorkspaceMutationFileSystem,
    ExecRunner: ProcessRunner,
> {
    families: WorkspaceBoundFamilies<FileSystem, ExecRunner>,
    roots: SessionWorkspaceRoots,
    git_identity: GitIdentity,
    exec_runner: ExecRunner,
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
        let exec_runner = TokioProcessRunner::try_new(exec_supervisor_executable)
            .map_err(|_| DaemonToolsConstructionError::Exec)?;
        let workspace_bound = ConfiguredWorkspaceComposition {
            families: WorkspaceBoundFamilies::try_new(
                workspace,
                workspace_root,
                git_identity.clone(),
                exec_runner.clone(),
            )?,
            roots: SessionWorkspaceRoots::new(workspace_root),
            git_identity,
            exec_runner,
        };
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
                workspace_bound: Some(workspace_bound),
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
                workspace_bound: None,
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
    FileSystem: WorkspaceFileSystem + WorkspaceMutationFileSystem + PinFurtherWorkspaceRoot,
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
        let workspace_bound = ConfiguredWorkspaceComposition {
            families: WorkspaceBoundFamilies::try_new(
                filesystem,
                workspace_root,
                git_identity.clone(),
                exec_runner.clone(),
            )?,
            roots: SessionWorkspaceRoots::new(workspace_root),
            git_identity,
            exec_runner,
        };
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
                workspace_bound: Some(workspace_bound),
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
            workspace_bound,
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
        catalogs.extend(
            workspace_bound
                .iter()
                .flat_map(|composition| composition.families.catalogs.iter().cloned()),
        );
        catalogs.extend(conversations.as_ref().map(|(catalog, _)| catalog.clone()));
        catalogs.extend(goal.as_ref().map(|(catalog, _)| catalog.clone()));
        let catalog = DaemonToolCatalog::try_new(catalogs)
            .map_err(|_| DaemonToolsConstructionError::Duplicate)?;
        let workspace_bound = workspace_bound
            .map(SessionWorkspaceExecutors::try_new)
            .transpose()?;
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
                workspace_bound,
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
    /// The sanitized detail reported when a session's derived workspace cannot
    /// be composed was itself invalid.
    SessionWorkspaceDetail,
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
            Self::SessionWorkspaceDetail => "session workspace failure detail was invalid",
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

/// Why one session's workspace-bound tools could not be composed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SessionWorkspaceFailure {
    /// The derived root, its repository layout, or its supervisor binding was
    /// rejected by the family that binds it.
    Composition(DaemonToolsConstructionError),
    /// The derived repository selects another object identifier format than the
    /// one the process-lifetime catalog compiled its Git validators against.
    ObjectFormatDisagreement,
}

impl SessionWorkspaceFailure {
    /// Names the failure for startup-free runtime telemetry.
    const fn discriminant(self) -> &'static str {
        match self {
            Self::Composition(_) => "composition_rejected",
            Self::ObjectFormatDisagreement => "object_format_disagreement",
        }
    }
}

/// One retained per-session value and the counter that orders eviction.
struct RetainedSessionWorkspace<Executors> {
    executors: Executors,
    last_used: u64,
}

/// Bounded set of derived per-session executor sets, keyed by session.
///
/// Generic in what it retains so the bound and the eviction order can be
/// exercised without composing real descriptor-holding executors.
struct RetainedSessionWorkspaces<Executors> {
    retained: BTreeMap<SessionId, RetainedSessionWorkspace<Executors>>,
    next_use: u64,
}

impl<Executors: Clone> RetainedSessionWorkspaces<Executors> {
    const fn new() -> Self {
        Self {
            retained: BTreeMap::new(),
            next_use: 0,
        }
    }

    fn take_use(&mut self) -> u64 {
        let use_order = self.next_use;
        self.next_use = self.next_use.saturating_add(1);
        use_order
    }

    fn get(&mut self, session: SessionId) -> Option<Executors> {
        let use_order = self.take_use();
        let retained = self.retained.get_mut(&session)?;
        retained.last_used = use_order;
        Some(retained.executors.clone())
    }

    /// Retains one composed set, dropping the least recently used entry when
    /// the bound is already reached, and returns the set now retained.
    ///
    /// A concurrent resolution for the same session may have retained its own
    /// set first; that one wins, so every caller converges on one pinned
    /// instance and the loser's descriptors are released immediately.
    fn retain(&mut self, session: SessionId, executors: Executors) -> Executors {
        if let Some(already_retained) = self.get(session) {
            return already_retained;
        }
        if self.retained.len() >= MAX_RETAINED_SESSION_WORKSPACES {
            let evicted = self
                .retained
                .iter()
                .min_by_key(|(_, retained)| retained.last_used)
                .map(|(session, _)| *session);
            if let Some(evicted) = evicted {
                self.retained.remove(&evicted);
            }
        }
        let last_used = self.take_use();
        self.retained.insert(
            session,
            RetainedSessionWorkspace {
                executors: executors.clone(),
                last_used,
            },
        );
        executors
    }
}

/// Resolves the workspace-bound executors one session's tool calls dispatch to.
///
/// The configured root's own set is composed at startup and shared by every
/// session whose derived root is absent, so an unprovisioned deployment keeps
/// exactly the composition, descriptors, and failure timing it had before.
struct SessionWorkspaceExecutors<FileSystem: WorkspaceMutationFileSystem, ExecRunner: ProcessRunner>
{
    roots: SessionWorkspaceRoots,
    git_identity: GitIdentity,
    exec_runner: ExecRunner,
    configured: WorkspaceBoundExecutors<FileSystem, ExecRunner>,
    unavailable_detail: ToolExecutionErrorDetail,
    retained:
        Arc<Mutex<RetainedSessionWorkspaces<WorkspaceBoundExecutors<FileSystem, ExecRunner>>>>,
}

impl<FileSystem: WorkspaceMutationFileSystem, ExecRunner: ProcessRunner> Clone
    for SessionWorkspaceExecutors<FileSystem, ExecRunner>
{
    fn clone(&self) -> Self {
        Self {
            roots: self.roots.clone(),
            git_identity: self.git_identity.clone(),
            exec_runner: self.exec_runner.clone(),
            configured: self.configured.clone(),
            unavailable_detail: self.unavailable_detail.clone(),
            retained: Arc::clone(&self.retained),
        }
    }
}

impl<FileSystem: WorkspaceMutationFileSystem, ExecRunner: ProcessRunner> fmt::Debug
    for SessionWorkspaceExecutors<FileSystem, ExecRunner>
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionWorkspaceExecutors")
            .finish_non_exhaustive()
    }
}

impl<FileSystem, ExecRunner> SessionWorkspaceExecutors<FileSystem, ExecRunner>
where
    FileSystem: WorkspaceFileSystem + WorkspaceMutationFileSystem + PinFurtherWorkspaceRoot,
    ExecRunner: ProcessRunner,
{
    fn try_new(
        composition: ConfiguredWorkspaceComposition<FileSystem, ExecRunner>,
    ) -> Result<Self, DaemonToolsConstructionError> {
        let ConfiguredWorkspaceComposition {
            families,
            roots,
            git_identity,
            exec_runner,
        } = composition;
        let unavailable_detail =
            ToolExecutionErrorDetail::try_new(SESSION_WORKSPACE_UNAVAILABLE_DETAIL.to_owned())
                .map_err(|_| DaemonToolsConstructionError::SessionWorkspaceDetail)?;
        Ok(Self {
            roots,
            git_identity,
            exec_runner,
            configured: families.executors,
            unavailable_detail,
            retained: Arc::new(Mutex::new(RetainedSessionWorkspaces::new())),
        })
    }

    async fn resolve(
        &mut self,
        session: SessionId,
    ) -> Result<WorkspaceBoundExecutors<FileSystem, ExecRunner>, SessionWorkspaceFailure> {
        // The retained set is consulted before the derivation so a session that
        // already bound a derived root keeps binding it: a directory removed
        // under a live session then yields that root's own typed failures
        // rather than silently returning the session to the configured root.
        if let Some(retained) = self.retained.lock().await.get(session) {
            return Ok(retained);
        }
        let path = match self.roots.resolve(session) {
            SessionWorkspaceRoot::ConfiguredRoot => return Ok(self.configured.clone()),
            SessionWorkspaceRoot::Derived { path } => path,
        };
        let filesystem = FileSystem::pin_further_root(&path).map_err(|_| {
            SessionWorkspaceFailure::Composition(DaemonToolsConstructionError::WorkspaceRead)
        })?;
        let families = WorkspaceBoundFamilies::try_new(
            filesystem,
            &path,
            self.git_identity.clone(),
            self.exec_runner.clone(),
        )
        .map_err(SessionWorkspaceFailure::Composition)?;
        if families.executors.git_object_format != self.configured.git_object_format {
            return Err(SessionWorkspaceFailure::ObjectFormatDisagreement);
        }
        Ok(self
            .retained
            .lock()
            .await
            .retain(session, families.executors))
    }

    /// Dispatches one workspace-root-bound request to the requesting session's
    /// own executors.
    ///
    /// An unresolvable session workspace closes the attempt as a known tool
    /// failure carrying sanitized detail — the model, the transcript, and both
    /// clients see it — beside one telemetry event naming the session and a
    /// closed reason. It is never silently redirected to another session's root.
    async fn execute(
        &mut self,
        invocation: ToolExecutionInvocation,
    ) -> Result<CorrelatedToolExecutorEvidence, DaemonToolExecutorError> {
        let session = invocation.correlation().session();
        let mut executors = match self.resolve(session).await {
            Ok(executors) => executors,
            Err(failure) => {
                tracing::warn!(
                    session_id = %session.into_uuid(),
                    reason = failure.discriminant(),
                    "session workspace tools are unavailable"
                );
                return Ok(invocation.bind(ToolExecutorEvidence::KnownFailed {
                    detail: Some(self.unavailable_detail.clone()),
                }));
            }
        };
        match invocation.request().name().as_str() {
            name if WORKSPACE_READ_TOOL_NAMES.contains(&name) => executors
                .workspace_read
                .execute(invocation)
                .await
                .map_err(|error| DaemonToolExecutorError::from_error(&error)),
            name if WORKSPACE_MUTATION_TOOL_NAMES.contains(&name) => executors
                .workspace_mutation
                .execute(invocation)
                .await
                .map_err(|error| DaemonToolExecutorError::from_error(&error)),
            name if LOCAL_GIT_TOOL_NAMES.contains(&name) => executors
                .local_git
                .execute(invocation)
                .await
                .map_err(|error| DaemonToolExecutorError::from_error(&error)),
            SANDBOXED_EXEC_NAME => executors
                .sandboxed_exec
                .execute(invocation)
                .await
                .map_err(|error| DaemonToolExecutorError::from_error(&error)),
            UNSANDBOXED_EXEC_NAME => executors
                .unsandboxed_exec
                .execute(invocation)
                .await
                .map_err(|error| DaemonToolExecutorError::from_error(&error)),
            CARGO_DIAGNOSTICS_NAME => executors
                .cargo_diagnostics
                .execute(invocation)
                .await
                .map_err(|error| DaemonToolExecutorError::from_error(&error)),
            _ => Err(DaemonToolExecutorError::unknown_tool()),
        }
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
    workspace_bound: Option<SessionWorkspaceExecutors<FileSystem, ExecRunner>>,
    conversations: Option<ConversationExecutor<ConversationPort>>,
    plan: PlanExecutor<PlanPort>,
    delegation: SessionDelegationExecutor<DaemonSessionDelegationPort>,
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
            name if WORKSPACE_READ_TOOL_NAMES.contains(&name)
                || WORKSPACE_MUTATION_TOOL_NAMES.contains(&name)
                || LOCAL_GIT_TOOL_NAMES.contains(&name)
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
    use std::{fmt, fs, time::SystemTime};

    use signalbox_application::{
        FixtureToolExecutionTransaction, FixtureTransactionFailures, InProcessToolDispatchGate,
        PreparedAttemptApproval, PreparedAttemptIdentities, PreparedAttemptProposal,
        RecordingToolExecutor, ToolCatalog, ToolExecutionService, UuidV7ToolLoopIdGenerator,
        prepared_single_attempt_batch,
    };
    use signalbox_domain::{
        ContextFrontierId, DurableCommandId, ModelCallId, ToolAttemptId, ToolEffectClass,
        ToolRequestId, TurnAttemptId, TurnId,
    };
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
                workspace_bound: None::<
                    ConfiguredWorkspaceComposition<LocalWorkspaceFileSystem, TokioProcessRunner>,
                >,
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
        offline_daemon_composition(workspace).0
    }

    /// Composes every injected family against offline boundaries and returns
    /// both composition roles.
    fn offline_daemon_composition(
        workspace: &Path,
    ) -> (
        DaemonToolCatalog,
        impl ToolExecutor<Error = DaemonToolExecutorError> + Clone + Send,
    ) {
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
            signalbox_domain::ToolPermissionDefault::Confirm
        );
        assert_eq!(
            permission_default(CARGO_DIAGNOSTICS_NAME),
            signalbox_domain::ToolPermissionDefault::Auto
        );
        assert_eq!(
            permission_default(UNSANDBOXED_EXEC_NAME),
            signalbox_domain::ToolPermissionDefault::AlwaysConfirm
        );
    }

    /// The two sessions the per-session workspace tests give separate roots.
    /// Each value only needs to be distinct from the other.
    const FIRST_SESSION_IDENTITY: u128 = 0x5001;
    const SECOND_SESSION_IDENTITY: u128 = 0x5002;
    /// Identities every driven fixture batch reuses. The session is the only
    /// axis these tests vary, so the rest are arbitrary but distinct.
    const FIXTURE_TURN_IDENTITY: u128 = 0x7001;
    const FIXTURE_PRODUCING_CALL_IDENTITY: u128 = 0x7002;
    const FIXTURE_REQUEST_IDENTITY: u128 = 0x7003;
    const FIXTURE_ATTEMPT_IDENTITY: u128 = 0x7004;
    const FIXTURE_ISSUING_TURN_ATTEMPT_IDENTITY: u128 = 0x7005;
    const FIXTURE_FRONTIER_IDENTITY: u128 = 0x7006;
    const FIXTURE_APPROVAL_COMMAND_IDENTITY: u128 = 0x7007;

    /// The one relative path every session's fixture workspace carries, so the
    /// content a read returns is evidence of which root answered it.
    const SESSION_MARKER_PATH: &str = "marker.txt";
    const CONFIGURED_ROOT_MARKER: &str = "configured root content";
    const FIRST_SESSION_MARKER: &str = "first session content";
    const SECOND_SESSION_MARKER: &str = "second session content";
    const FIRST_SESSION_REPLACEMENT: &str = "first session replacement";

    fn session(identity: u128) -> SessionId {
        SessionId::from_uuid(uuid::Uuid::from_u128(identity))
    }

    /// Creates the configured root as a direct main worktree.
    fn configured_workspace(parent: &Path) -> PathBuf {
        let configured = parent.join("workspace");
        fs::create_dir(&configured).expect("configured workspace exists");
        git2::Repository::init(&configured).expect("configured repository initializes");
        fs::write(configured.join(SESSION_MARKER_PATH), CONFIGURED_ROOT_MARKER)
            .expect("configured marker is written");
        configured
    }

    /// Creates a direct main worktree exactly where the derivation places one
    /// session's root, so the test never restates the formula.
    fn provisioned_session_workspace(configured: &Path, session: SessionId, marker: &str) {
        let derived = SessionWorkspaceRoots::new(configured)
            .derived_path(session)
            .expect("the fixture configured root has a parent and a final component");
        fs::create_dir_all(&derived).expect("derived session workspace exists");
        git2::Repository::init(&derived).expect("derived session repository initializes");
        fs::write(derived.join(SESSION_MARKER_PATH), marker)
            .expect("derived session marker is written");
    }

    fn read_marker_proposal() -> PreparedAttemptProposal {
        PreparedAttemptProposal {
            name: ToolName::try_new(String::from(READ_FILE_NAME))
                .expect("read_file is a valid tool name"),
            arguments: arguments(&serde_json::json!({"path": SESSION_MARKER_PATH}).to_string()),
            effect_class: ToolEffectClass::EffectFree,
            approval: PreparedAttemptApproval::PolicyAuto,
        }
    }

    fn write_marker_proposal(content: &str) -> PreparedAttemptProposal {
        PreparedAttemptProposal {
            name: ToolName::try_new(String::from(WRITE_FILE_NAME))
                .expect("write_file is a valid tool name"),
            arguments: arguments(
                &serde_json::json!({"path": SESSION_MARKER_PATH, "content": content}).to_string(),
            ),
            effect_class: ToolEffectClass::ExternalEffect,
            // `write_file` is declared `Confirm`, so a policy approval would
            // describe a batch the application never prepares for it.
            approval: PreparedAttemptApproval::UserConfirmation {
                command: DurableCommandId::from_uuid(uuid::Uuid::from_u128(
                    FIXTURE_APPROVAL_COMMAND_IDENTITY,
                )),
            },
        }
    }

    fn arguments(value: &str) -> NormalizedToolArguments {
        NormalizedToolArguments::try_from_provider_text(value.to_owned())
            .expect("fixture arguments are admitted")
    }

    /// Drives one prepared single-attempt batch through the real tool-execution
    /// service and returns the evidence the daemon executor bound.
    ///
    /// `ToolExecutionInvocation` has no public constructor, so the session on
    /// the invocation the executor reads can only be established by running a
    /// real batch for that session.
    async fn daemon_evidence<Executor>(
        catalog: DaemonToolCatalog,
        executor: Executor,
        session: SessionId,
        proposal: PreparedAttemptProposal,
    ) -> ToolExecutorEvidence
    where
        Executor: ToolExecutor<Error = DaemonToolExecutorError> + Send,
    {
        let (executor, recorded) = RecordingToolExecutor::new(executor);
        let batch = prepared_single_attempt_batch(
            PreparedAttemptIdentities {
                session,
                turn: TurnId::from_uuid(uuid::Uuid::from_u128(FIXTURE_TURN_IDENTITY)),
                producing_call: ModelCallId::from_uuid(uuid::Uuid::from_u128(
                    FIXTURE_PRODUCING_CALL_IDENTITY,
                )),
                request: ToolRequestId::from_uuid(uuid::Uuid::from_u128(FIXTURE_REQUEST_IDENTITY)),
                attempt: ToolAttemptId::from_uuid(uuid::Uuid::from_u128(FIXTURE_ATTEMPT_IDENTITY)),
                issuing_turn_attempt: TurnAttemptId::from_uuid(uuid::Uuid::from_u128(
                    FIXTURE_ISSUING_TURN_ATTEMPT_IDENTITY,
                )),
                frontier: ContextFrontierId::from_uuid(uuid::Uuid::from_u128(
                    FIXTURE_FRONTIER_IDENTITY,
                )),
            },
            proposal,
        );
        let mut service = ToolExecutionService::new(
            UuidV7ToolLoopIdGenerator,
            FixtureToolExecutionTransaction::new(
                batch.clone(),
                // Neither failure is reachable from a coherent prepared batch;
                // the daemon executor's only sanitized value stands in for both
                // so an unexpected route reports as a caller-or-hub bug.
                FixtureTransactionFailures {
                    domain_rejection: DaemonToolExecutorError::unknown_tool(),
                    declined_crash_classification: DaemonToolExecutorError::unknown_tool(),
                },
            ),
            catalog,
            executor,
            InProcessToolDispatchGate::default(),
        );

        service
            .execute(batch.session(), batch.turn())
            .await
            .expect("the prepared attempt commits definitive evidence");

        recorded
            .take()
            .expect("the daemon executor bound evidence for the prepared attempt")
    }

    #[track_caller]
    fn completed_text(evidence: ToolExecutorEvidence) -> String {
        match evidence {
            ToolExecutorEvidence::CompletedText(text) => text,
            ToolExecutorEvidence::KnownFailed { detail } => {
                panic!("the workspace tool failed: {detail:?}")
            }
            ToolExecutorEvidence::Ambiguous => panic!("the workspace tool was ambiguous"),
        }
    }

    #[track_caller]
    fn read_content(evidence: ToolExecutorEvidence) -> String {
        let decoded: serde_json::Value =
            serde_json::from_str(&completed_text(evidence)).expect("read_file evidence is JSON");
        decoded["content"]
            .as_str()
            .expect("read_file evidence carries string content")
            .to_owned()
    }

    /// The derivation places every session's root beside the configured root
    /// rather than inside it, so a session still bound to the configured root
    /// cannot read, write, or execute another session's tree.
    #[test]
    fn a_session_workspace_is_derived_beside_the_configured_root() {
        let configured = Path::new("/srv/signalbox/workspace");
        let first = session(FIRST_SESSION_IDENTITY);

        let derived = SessionWorkspaceRoots::new(configured)
            .derived_path(first)
            .expect("an absolute configured root has a parent and a final component");

        assert_eq!(
            derived,
            Path::new("/srv/signalbox/workspace.sessions").join(first.into_uuid().to_string())
        );
    }

    /// A session with no provisioned directory binds the configured root, which
    /// is exactly what every session bound before this derivation existed.
    #[test]
    fn an_unprovisioned_session_binds_the_configured_root() {
        let parent = tempfile::tempdir().expect("fixture parent exists");
        let configured = configured_workspace(parent.path());

        let resolved =
            SessionWorkspaceRoots::new(&configured).resolve(session(FIRST_SESSION_IDENTITY));

        assert_eq!(resolved, SessionWorkspaceRoot::ConfiguredRoot);
    }

    /// A session whose derived directory exists binds that directory.
    #[test]
    fn a_provisioned_session_binds_its_derived_root() {
        let parent = tempfile::tempdir().expect("fixture parent exists");
        let configured = configured_workspace(parent.path());
        let first = session(FIRST_SESSION_IDENTITY);
        provisioned_session_workspace(&configured, first, FIRST_SESSION_MARKER);
        let roots = SessionWorkspaceRoots::new(&configured);
        let expected = roots
            .derived_path(first)
            .expect("the fixture configured root has a parent and a final component");

        let resolved = roots.resolve(first);

        assert_eq!(resolved, SessionWorkspaceRoot::Derived { path: expected });
    }

    /// One composition serves two concurrent sessions from two roots: each
    /// session's `read_file` observes only its own workspace, and neither
    /// observes the configured root every session shared before.
    #[tokio::test]
    async fn two_sessions_read_only_their_own_derived_workspace() {
        let parent = tempfile::tempdir().expect("fixture parent exists");
        let configured = configured_workspace(parent.path());
        let first = session(FIRST_SESSION_IDENTITY);
        let second = session(SECOND_SESSION_IDENTITY);
        provisioned_session_workspace(&configured, first, FIRST_SESSION_MARKER);
        provisioned_session_workspace(&configured, second, SECOND_SESSION_MARKER);
        let (catalog, executor) = offline_daemon_composition(&configured);

        let first_read = daemon_evidence(
            catalog.clone(),
            executor.clone(),
            first,
            read_marker_proposal(),
        )
        .await;
        let second_read = daemon_evidence(catalog, executor, second, read_marker_proposal()).await;

        assert_eq!(read_content(first_read), FIRST_SESSION_MARKER);
        assert_eq!(read_content(second_read), SECOND_SESSION_MARKER);
    }

    /// A write through one session's workspace tools reaches only that
    /// session's root: the other session's read is unchanged, and so is the
    /// configured root's own copy of the same relative path.
    #[tokio::test]
    async fn a_session_write_is_invisible_to_another_session() {
        let parent = tempfile::tempdir().expect("fixture parent exists");
        let configured = configured_workspace(parent.path());
        let first = session(FIRST_SESSION_IDENTITY);
        let second = session(SECOND_SESSION_IDENTITY);
        provisioned_session_workspace(&configured, first, FIRST_SESSION_MARKER);
        provisioned_session_workspace(&configured, second, SECOND_SESSION_MARKER);
        let (catalog, executor) = offline_daemon_composition(&configured);

        let write = daemon_evidence(
            catalog.clone(),
            executor.clone(),
            first,
            write_marker_proposal(FIRST_SESSION_REPLACEMENT),
        )
        .await;
        let first_read = daemon_evidence(
            catalog.clone(),
            executor.clone(),
            first,
            read_marker_proposal(),
        )
        .await;
        let second_read = daemon_evidence(catalog, executor, second, read_marker_proposal()).await;

        // Panics unless the write completed; what it returned is not the claim.
        completed_text(write);
        assert_eq!(read_content(first_read), FIRST_SESSION_REPLACEMENT);
        assert_eq!(read_content(second_read), SECOND_SESSION_MARKER);
        assert_eq!(
            fs::read_to_string(configured.join(SESSION_MARKER_PATH))
                .expect("the configured marker is still readable"),
            CONFIGURED_ROOT_MARKER
        );
    }

    /// Two adapters pinned to two roots each answer for their own root, which
    /// is what makes one composition able to serve two sessions at once.
    #[test]
    fn two_pinned_filesystems_answer_for_their_own_roots() {
        let parent = tempfile::tempdir().expect("fixture parent exists");
        let first_root = parent.path().join("first");
        let second_root = parent.path().join("second");
        fs::create_dir(&first_root).expect("first fixture workspace exists");
        fs::create_dir(&second_root).expect("second fixture workspace exists");
        fs::write(first_root.join(SESSION_MARKER_PATH), FIRST_SESSION_MARKER)
            .expect("first fixture content is written");
        fs::write(second_root.join(SESSION_MARKER_PATH), SECOND_SESSION_MARKER)
            .expect("second fixture content is written");
        let first = PinnedWorkspaceFileSystem::pin_further_root(&first_root)
            .expect("the first root is pinned");
        let second = PinnedWorkspaceFileSystem::pin_further_root(&second_root)
            .expect("the second root is pinned");

        // Both adapters are handed the *same* path, so what each returns is
        // evidence of the root it pinned and not of the path it was given.
        let first_read = WorkspaceFileSystem::read_file_prefix(
            &first,
            &WorkspaceFileSystem::open_root(&first, &second_root)
                .expect("the first adapter returns its own pinned root"),
            Path::new(SESSION_MARKER_PATH),
            FIRST_SESSION_MARKER.len(),
        )
        .expect("the first adapter reads its own root");
        let second_read = WorkspaceFileSystem::read_file_prefix(
            &second,
            &WorkspaceFileSystem::open_root(&second, &first_root)
                .expect("the second adapter returns its own pinned root"),
            Path::new(SESSION_MARKER_PATH),
            SECOND_SESSION_MARKER.len(),
        )
        .expect("the second adapter reads its own root");

        assert_eq!(first_read.bytes, FIRST_SESSION_MARKER.as_bytes());
        assert_eq!(second_read.bytes, SECOND_SESSION_MARKER.as_bytes());
    }

    /// Sessions whose only role is to occupy the retained set's remaining
    /// capacity. Distinct from the two named sessions and from each other.
    const FILLER_SESSION_IDENTITY_BASE: u128 = 0x6000;

    /// Retains filler sessions until the bound is reached.
    ///
    /// The iteration lives here rather than in a test body, which stays
    /// straight-line: what the test is about is which entry the next retention
    /// evicts, not how the set was filled.
    fn fill_remaining_capacity(retained: &mut RetainedSessionWorkspaces<u32>, filler: u32) {
        for offset in 0..MAX_RETAINED_SESSION_WORKSPACES - 2 {
            let identity = FILLER_SESSION_IDENTITY_BASE + offset as u128;
            retained.retain(session(identity), filler);
        }
    }

    /// The retained set is bounded: a further session drops the least recently
    /// used entry rather than growing the descriptor count without limit.
    #[test]
    fn retaining_beyond_the_bound_evicts_the_least_recently_used_session() {
        const FIRST_RETAINED: u32 = 1;
        const SECOND_RETAINED: u32 = 2;
        const OVERFLOWING_RETAINED: u32 = 3;
        let mut retained = RetainedSessionWorkspaces::new();
        let first = session(FIRST_SESSION_IDENTITY);
        let second = session(SECOND_SESSION_IDENTITY);
        let overflowing = session(FILLER_SESSION_IDENTITY_BASE - 1);
        retained.retain(first, FIRST_RETAINED);
        retained.retain(second, SECOND_RETAINED);
        fill_remaining_capacity(&mut retained, FIRST_RETAINED);
        // Reading `second` back makes it the most recently used entry, so the
        // entry the overflowing session evicts is unambiguously `first`.
        assert_eq!(retained.get(second), Some(SECOND_RETAINED));

        retained.retain(overflowing, OVERFLOWING_RETAINED);

        assert_eq!(retained.get(first), None);
        assert_eq!(retained.get(second), Some(SECOND_RETAINED));
        assert_eq!(retained.get(overflowing), Some(OVERFLOWING_RETAINED));
    }
}
