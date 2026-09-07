//! Process-lifetime compiled daemon tool catalog and executor dispatch.
//!
//! The catalog is one process-lifetime immutable compiled value; the executors
//! a workspace root binds are per session, resolved through
//! [`SessionWorkspaceRoots`]. See `docs/spec/tool-loop.md` and
//! `docs/spec/git-authority-threat-model.md`.

mod composed_identity;
mod pinned_file_system;
mod session_workspace_roots;
mod shared_executor;
#[cfg(test)]
mod tests;
mod workspace_executors;
mod workspace_failure;

use crate::{
    FileCredentialAccess, PostgresConversationIntrospection,
    blob_tools::{BLOB_METADATA_NAME, BLOB_READ_NAME, BLOB_TOOL_NAMES, BlobToolExecutor},
    goal_mode::{GOAL_DECLARE_NAME, GoalDeclarationExecutor, GoalDeclarationTool},
    session_delegation::DaemonSessionDelegationPort,
};
pub use composed_identity::ComposedWorkspaceIdentity;
pub use pinned_file_system::{PinFurtherWorkspaceRoot, PinnedWorkspaceFileSystem};
#[cfg(test)]
use session_workspace_roots::{
    ComposedRootIdentity, GIT_ADMINISTRATION_DIRECTORY, SESSION_WORKSPACE_REPLACED_DETAIL,
    SESSION_WORKSPACE_UNVERIFIABLE_CONFIGURED_DETAIL, SessionRootDecision, SessionWorkspaceRoot,
    WorkspaceInstructionRootResolutionError, a_derived_binding_exists,
    a_derived_binding_shares_the_configured_root, another_session_bound,
    composition_aliases_its_own_parent, decide_session_root, parent_aliases_the_configured_root,
    probe_is_stale, shares_a_directory_with_the_configured_root,
};
use session_workspace_roots::{
    MAX_RETAINED_SESSION_WORKSPACES, RecordedSessionBinding, composed_root_identity,
};
pub use session_workspace_roots::{SessionWorkspaceRoots, WorkspaceInstructionRootResolver};
use shared_executor::SharedToolExecutor;
#[cfg(test)]
use signalbox_application::ToolExecutorEvidence;
use signalbox_application::{
    ClassifyOperatorFailure, CompiledToolCatalog, CorrelatedDurableChildWait,
    CorrelatedToolExecutorEvidence, OperatorFailureClass, ToolCatalog,
    ToolCatalogValidationFailure, ToolDefinition, ToolExecutionInvocation, ToolExecutor,
    ToolExecutorDisposition,
};
use signalbox_domain::{NormalizedToolArguments, SessionId, ToolApprovalPosture, ToolName};
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
#[cfg(test)]
use signalbox_tools_workspace::{LocalWorkspaceFileSystem, WorkspaceMutationPath};
use signalbox_tools_workspace::{
    WORKSPACE_MUTATION_TOOL_NAMES, WORKSPACE_READ_TOOL_NAMES, WorkspaceFileSystem,
    WorkspaceMutationExecutor, WorkspaceMutationFileSystem, WorkspaceMutationTools,
    WorkspaceReadExecutor, WorkspaceReadTools,
};
use sqlx::PgPool;
use std::{
    collections::BTreeMap,
    error::Error,
    fmt,
    path::{Path, PathBuf},
};
use workspace_executors::SessionWorkspaceExecutors;

/// The six executors one workspace root binds.
struct WorkspaceBoundExecutors<FileSystem: WorkspaceMutationFileSystem, ExecRunner: ProcessRunner> {
    workspace_read: WorkspaceReadExecutor<FileSystem>,
    workspace_mutation: SharedToolExecutor<WorkspaceMutationExecutor<FileSystem>>,
    local_git: SharedToolExecutor<LocalGitExecutor<FileSystem>>,
    sandboxed_exec: ExecExecutor<SandboxedCommandRunner<ExecRunner>>,
    unsandboxed_exec: ExecExecutor<UnsandboxedCommandRunner<ExecRunner>>,
    cargo_diagnostics: CargoDiagnosticsExecutor<ExecRunner>,
    git_object_format: GitObjectFormat,
    workspace_identity: ComposedWorkspaceIdentity,
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
            workspace_identity: self.workspace_identity,
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
        cargo_registry_cache: Option<&Path>,
    ) -> Result<Self, DaemonToolsConstructionError> {
        // Each family below resolves the same pathname independently, so a
        // rename or replacement between two of them would leave one family
        // bound to the old directory and another to its replacement. The
        // identity is captured on both sides of the composition and compared
        // before anything is returned, so a pathname that did not resolve to
        // one directory throughout rejects the whole composition.
        let opening_identity = composed_root_identity(root)?;
        let workspace_read = WorkspaceReadTools::try_new(filesystem.clone(), root)
            .map_err(|_| DaemonToolsConstructionError::WorkspaceRead)?;
        let workspace_mutation = WorkspaceMutationTools::try_new(filesystem.clone(), root)
            .map_err(|_| DaemonToolsConstructionError::WorkspaceMutation)?;
        let local_git =
            LocalGitTools::try_new(filesystem, root, git_identity).map_err(|error| {
                tracing::error!(
                    cause = %error,
                    cause_detail = ?error,
                    workspace_root = %root.display(),
                    "local Git tool suite rejected the configured workspace"
                );
                DaemonToolsConstructionError::LocalGit
            })?;
        let git_object_format = local_git.object_format();
        let pinned_directories = local_git.pinned_directories();
        let sandboxed_exec = match cargo_registry_cache {
            Some(cache) => {
                SandboxedExecTool::try_new_with_cargo_registry(exec_runner.clone(), root, cache)
            }
            None => SandboxedExecTool::try_new(exec_runner.clone(), root),
        }
        .map_err(|_| DaemonToolsConstructionError::Exec)?;
        let unsandboxed_exec = UnsandboxedExecTool::try_new(exec_runner.clone(), root)
            .map_err(|_| DaemonToolsConstructionError::Exec)?;
        let cargo_diagnostics = match cargo_registry_cache {
            Some(cache) => {
                CargoDiagnosticsTool::try_new_with_cargo_registry(exec_runner, root, cache)
            }
            None => CargoDiagnosticsTool::try_new(exec_runner, root),
        }
        .map_err(|_| DaemonToolsConstructionError::Exec)?;
        let (workspace_read_catalog, workspace_read) = workspace_read.into_parts();
        let (workspace_mutation_catalog, workspace_mutation) = workspace_mutation.into_parts();
        let (local_git_catalog, local_git) = local_git.into_parts();
        let (sandboxed_exec_catalog, sandboxed_exec) = sandboxed_exec.into_parts();
        let (unsandboxed_exec_catalog, unsandboxed_exec) = unsandboxed_exec.into_parts();
        let (cargo_diagnostics_catalog, cargo_diagnostics) = cargo_diagnostics.into_parts();
        // The Git suite is the only family that pins a second directory, and it
        // pinned the one it validated rather than the one this pathname names
        // now, so the composition's recorded identity is taken from it. Its
        // worktree root is still compared against the pathname every other
        // family resolved, so a Git suite bound to another directory than the
        // rest of the composition rejects it.
        let workspace_identity = ComposedWorkspaceIdentity::from_pinned(pinned_directories);
        if composed_root_identity(root)? != opening_identity
            || workspace_identity.root != opening_identity
        {
            return Err(DaemonToolsConstructionError::WorkspaceRootUnstable);
        }
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
                workspace_identity,
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
    cargo_registry_cache: Option<PathBuf>,
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
        cargo_registry_cache: Option<&Path>,
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
                cargo_registry_cache,
            )?,
            roots: SessionWorkspaceRoots::try_new(workspace_root)?,
            git_identity,
            exec_runner,
            cargo_registry_cache: cargo_registry_cache.map(Path::to_path_buf),
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
                None,
            )?,
            roots: SessionWorkspaceRoots::try_new(workspace_root)?,
            git_identity,
            exec_runner,
            cargo_registry_cache: None,
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
                blob: None,
            },
        })
    }

    /// Shares the workspace-binding authority used by workspace-bound tools.
    pub fn workspace_instruction_root_resolver(&self) -> Option<WorkspaceInstructionRootResolver>
    where
        FileSystem: Send + Sync + 'static,
        ExecRunner: Send + Sync + 'static,
    {
        self.executor
            .workspace_bound
            .clone()
            .map(WorkspaceInstructionRootResolver::new)
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
    /// The workspace root pathname did not resolve to one directory for the
    /// whole composition, so the composed families could disagree about which
    /// directory they bound.
    WorkspaceRootUnstable,
    /// The configured workspace root has no lexical parent and final component,
    /// so the per-session derivation formula cannot be applied to it.
    WorkspaceRootUnderivable,
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
            Self::WorkspaceRootUnstable => {
                "workspace root changed identity during tool composition"
            }
            Self::WorkspaceRootUnderivable => {
                "workspace root has no final path component to derive session roots from"
            }
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

    /// Extends the immutable daemon registry with one compiled family.
    pub fn with_compiled_catalog(
        mut self,
        catalog: CompiledToolCatalog,
    ) -> Result<Self, DaemonToolsConstructionError> {
        for definition in catalog.definitions() {
            let name = definition.name().clone();
            if self
                .entries
                .insert(
                    name.clone(),
                    DaemonToolCatalogEntry {
                        definition,
                        catalog: catalog.clone(),
                    },
                )
                .is_some()
            {
                return Err(DaemonToolsConstructionError::Duplicate);
            }
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
        || BLOB_TOOL_NAMES.contains(&name)
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

    fn preauthorization(
        &self,
        name: &ToolName,
        arguments: &NormalizedToolArguments,
    ) -> Result<signalbox_application::ToolPreauthorization, ToolCatalogValidationFailure> {
        self.entries
            .get(name)
            .ok_or(ToolCatalogValidationFailure::UnknownTool)?
            .catalog
            .preauthorization(name, arguments)
    }
}

/// Whether a retained value is still reachable from a request in flight.
///
/// Releasing a value a request still holds does not stop that request: it lets
/// the next request for the same session compose a second value beside it, with
/// its own serialization domain. Two mutations of one tree would then run
/// concurrently under two different locks, which is exactly what per-session
/// serialization exists to prevent.
trait RetainedInFlight {
    /// Whether any handle outside the retained set still holds this value.
    fn is_in_flight(&self) -> bool;
}

impl<FileSystem: WorkspaceMutationFileSystem, ExecRunner: ProcessRunner> RetainedInFlight
    for WorkspaceBoundExecutors<FileSystem, ExecRunner>
{
    fn is_in_flight(&self) -> bool {
        // Only the two serializing families carry an identity a second
        // composition could duplicate. The read and execution families hold no
        // lock: a read observes a pinned descriptor, and every execution
        // revalidates the root's identity around its own launch.
        !self.workspace_mutation.is_sole_handle() || !self.local_git.is_sole_handle()
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

/// Every session's recorded binding beside the bounded set of composed
/// executors.
///
/// One lock covers both, because the binding a session is recorded with and the
/// executors retained for it are one fact: recording a derived binding while
/// another caller retained the configured composition would leave a session
/// holding two answers at once.
struct SessionWorkspaceState<Executors> {
    bindings: BTreeMap<SessionId, RecordedSessionBinding>,
    retained: RetainedSessionWorkspaces<Executors>,
}

impl<Executors: Clone + RetainedInFlight> SessionWorkspaceState<Executors> {
    const fn new() -> Self {
        Self {
            bindings: BTreeMap::new(),
            retained: RetainedSessionWorkspaces::new(),
        }
    }
}

impl<Executors: Clone + RetainedInFlight> RetainedSessionWorkspaces<Executors> {
    const fn new() -> Self {
        Self {
            retained: BTreeMap::new(),
            next_use: 0,
        }
    }

    /// Releases idle entries until the set is back under the bound, or until
    /// none is releasable.
    ///
    /// Releasing one entry per retention would leave the set permanently above
    /// the bound after a burst of concurrent sessions, since each later
    /// retention released one and inserted one. The excess an in-flight request
    /// forces is temporary only if it drains once those requests return.
    fn release_idle_overflow(&mut self) {
        while self.retained.len() >= MAX_RETAINED_SESSION_WORKSPACES {
            let releasable = self
                .retained
                .iter()
                .filter(|(_, retained)| !retained.executors.is_in_flight())
                .min_by_key(|(_, retained)| retained.last_used)
                .map(|(session, _)| *session);
            let Some(releasable) = releasable else {
                return;
            };
            self.retained.remove(&releasable);
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

    /// Retains one composed set, dropping the least recently used idle entry
    /// when the bound is already reached, and returns the set now retained.
    ///
    /// A concurrent resolution for the same session may have retained its own
    /// set first; that one wins, so every caller converges on one pinned
    /// instance and the loser's descriptors are released immediately.
    ///
    /// An entry a request still holds is not an eviction candidate, so the
    /// retained set may exceed the bound by the number of sessions executing a
    /// workspace-bound tool at that moment. That excess is what keeps one
    /// session's serialization domain single; it is released as soon as those
    /// requests return, at the next retention.
    fn retain(&mut self, session: SessionId, executors: Executors) -> Executors {
        if let Some(already_retained) = self.get(session) {
            return already_retained;
        }
        self.release_idle_overflow();
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
    blob: Option<BlobToolExecutor>,
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
        let name = invocation.request().name().as_str();
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
