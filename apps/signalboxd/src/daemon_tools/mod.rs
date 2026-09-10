//! Process-lifetime compiled daemon tool catalog and executor dispatch.
//!
//! The catalog is one process-lifetime immutable compiled value; the executors
//! a workspace root binds are per session, resolved through
//! [`SessionWorkspaceRoots`]. See `docs/spec/tool-loop.md` and
//! `docs/spec/git-authority-threat-model.md`.

mod catalog;
mod composed_identity;
mod executor;
mod families;
mod file_media;
pub use file_media::DaemonFileMediaExecutor;
mod git_push;
mod pinned_file_system;
mod retained_workspaces;
mod session_status;
mod session_workspace_roots;
mod shared_executor;
#[cfg(test)]
mod tests;
mod workspace_executors;
mod workspace_failure;

#[cfg(test)]
use crate::goal_mode::GOAL_DECLARE_NAME;
use crate::{
    FileCredentialAccess, PostgresConversationIntrospection, goal_mode::GoalDeclarationTool,
    session_delegation::DaemonSessionDelegationPort,
};
pub use catalog::{ConfiguredApprovalPostureError, DaemonToolCatalog, DaemonToolComposition};
#[cfg(test)]
use composed_identity::ComposedWorkspaceIdentity;
pub use executor::{DaemonToolExecutor, DaemonToolExecutorError};
pub use families::{BaseDaemonCredentialInputs, MappedDaemonCredentialInputs};
use families::{ComposedToolFamilies, ConfiguredWorkspaceComposition, WorkspaceBoundFamilies};
pub use pinned_file_system::{PinFurtherWorkspaceRoot, PinnedWorkspaceFileSystem};
#[cfg(test)]
use retained_workspaces::{RetainedInFlight, RetainedSessionWorkspaces};
pub use session_status::{PostgresSessionStatusWriter, PostgresSessionStatusWriterError};
#[cfg(test)]
use session_workspace_roots::{
    ComposedRootIdentity, GIT_ADMINISTRATION_DIRECTORY, MAX_RETAINED_SESSION_WORKSPACES,
    RecordedSessionBinding, SESSION_WORKSPACE_REPLACED_DETAIL,
    SESSION_WORKSPACE_UNVERIFIABLE_CONFIGURED_DETAIL, SessionRootDecision, SessionWorkspaceRoot,
    WorkspaceInstructionRootResolutionError, a_derived_binding_exists,
    a_derived_binding_shares_the_configured_root, another_session_bound,
    composition_aliases_its_own_parent, decide_session_root, parent_aliases_the_configured_root,
    probe_is_stale, shares_a_directory_with_the_configured_root,
};
pub use session_workspace_roots::{SessionWorkspaceRoots, WorkspaceInstructionRootResolver};
#[cfg(test)]
use shared_executor::SharedToolExecutor;
#[cfg(test)]
use signalbox_application::{
    ClassifyOperatorFailure, OperatorFailureClass, ToolDefinition, ToolExecutor,
    ToolExecutorEvidence,
};
#[cfg(test)]
use signalbox_domain::{NormalizedToolArguments, SessionId, ToolApprovalPosture, ToolName};
use signalbox_persistence::plan::SessionPlanRepository;
#[cfg(test)]
use signalbox_tools_basic::{
    CURRENT_TIME_NAME, ECHO_NAME, SESSION_STATUS_UPDATE_NAME, SessionStatusWriter,
};
use signalbox_tools_basic::{CurrentTimeTool, EchoTool, SessionStatusTool};
#[cfg(test)]
use signalbox_tools_code_host::CodeHostTransport;
use signalbox_tools_code_host::{CodeHostTools, GitHubCodeHostTransport};
#[cfg(test)]
use signalbox_tools_conversations::ConversationIntrospectionPort;
use signalbox_tools_conversations::ConversationTools;
#[cfg(test)]
use signalbox_tools_exec::{CARGO_DIAGNOSTICS_NAME, SANDBOXED_EXEC_NAME, UNSANDBOXED_EXEC_NAME};
use signalbox_tools_exec::{ProcessRunner, TokioProcessRunner};
use signalbox_tools_git::GitIdentity;
#[cfg(test)]
use signalbox_tools_github::GitHubTransport;
use signalbox_tools_github::{GitHubApiTransport, GitHubEgressPolicy, GitHubTools};
use signalbox_tools_plan::PlanTools;
#[cfg(test)]
use signalbox_tools_plan::SessionPlanPort;
use signalbox_tools_sessions::SessionDelegationTools;
use signalbox_tools_web::{
    ReqwestWebFetchTransport, ReqwestWebSearchTransport, WebFetchEgressPolicy, WebFetchTool,
    WebSearchConfiguration, WebSearchProvider, WebSearchTool,
};
#[cfg(test)]
use signalbox_tools_web::{WEB_FETCH_NAME, WEB_SEARCH_NAME, WebFetchTransport, WebSearchTransport};
#[cfg(test)]
use signalbox_tools_workspace::{LocalWorkspaceFileSystem, WorkspaceMutationPath};
use signalbox_tools_workspace::{WorkspaceFileSystem, WorkspaceMutationFileSystem};
use sqlx::PgPool;
#[cfg(test)]
use std::collections::BTreeMap;
use std::{error::Error, fmt, path::Path};
use workspace_executors::SessionWorkspaceExecutors;

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
        eligibility_nudge: signalbox_application::InProcessEligibilityNudge,
        credentials: MappedDaemonCredentialInputs<FileCredentialAccess>,
        code_host_transport: GitHubCodeHostTransport,
        github_egress_policy: GitHubEgressPolicy,
        workspace_root: &Path,
        git_identity: GitIdentity,
        exec_supervisor_executable: &Path,
        cargo_registry_cache: Option<&Path>,
        sandbox: &signalbox_tools_exec::SandboxConfiguration,
        max_git_object_bytes: Option<usize>,
        sandboxed_exec_timeout_bound: Option<std::time::Duration>,
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
        let status = SessionStatusTool::try_new(PostgresSessionStatusWriter::new(pool.clone()))
            .map_err(|_| DaemonToolsConstructionError::SessionStatus)?;
        let code_host = CodeHostTools::try_new(code_host, code_host_transport)
            .map_err(|_| DaemonToolsConstructionError::CodeHost)?;
        let github_transport = GitHubApiTransport::try_new()
            .map_err(|_| DaemonToolsConstructionError::GitHub)?
            .with_app(github.github_app());
        let github = github.with_request_timeout(Some(github_transport.request_timeout()));
        let github = GitHubTools::try_new(github, github_transport, github_egress_policy)
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
                sandbox,
                max_git_object_bytes,
                sandboxed_exec_timeout_bound,
            )?,
            roots: SessionWorkspaceRoots::try_new(workspace_root)?,
            git_identity,
            exec_runner,
            cargo_registry_cache: cargo_registry_cache.map(Path::to_path_buf),
            sandbox: sandbox.clone(),
            max_git_object_bytes,
            sandboxed_exec_timeout_bound,
        };
        let conversations =
            ConversationTools::try_new(PostgresConversationIntrospection::new(pool.clone()))
                .map_err(|_| DaemonToolsConstructionError::Conversations)?;
        let goal = GoalDeclarationTool::try_new(pool.clone())
            .map_err(|_| DaemonToolsConstructionError::GoalDeclaration)?;
        let delegation = SessionDelegationTools::try_new(DaemonSessionDelegationPort::postgres(
            pool.clone(),
            eligibility_nudge,
        ))
        .map_err(|_| DaemonToolsConstructionError::SessionDelegation)?;
        let plan = PlanTools::try_new(SessionPlanRepository::new(pool.clone()))
            .map_err(|_| DaemonToolsConstructionError::Plan)?;
        let mut tools = Self::try_new_with_tools(
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
        )?;
        if let Some(workspace) = &mut tools.executor.workspace_bound {
            workspace.binding_pool = Some(pool);
        }
        Ok(tools)
    }

    /// Composes the base production catalog without constructing any dependency
    /// owned by an unconfigured tool family.
    pub fn try_new_without_tool_mappings(
        clock: Clock,
        pool: PgPool,
        eligibility_nudge: signalbox_application::InProcessEligibilityNudge,
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
        let status = SessionStatusTool::try_new(PostgresSessionStatusWriter::new(pool.clone()))
            .map_err(|_| DaemonToolsConstructionError::SessionStatus)?;
        let goal = GoalDeclarationTool::try_new(pool.clone())
            .map_err(|_| DaemonToolsConstructionError::GoalDeclaration)?;
        let code_host = CodeHostTools::try_new(code_host, code_host_transport)
            .map_err(|_| DaemonToolsConstructionError::CodeHost)?;
        let delegation = SessionDelegationTools::try_new(DaemonSessionDelegationPort::postgres(
            pool.clone(),
            eligibility_nudge,
        ))
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
                &Default::default(),
                None,
                None,
            )?,
            roots: SessionWorkspaceRoots::try_new(workspace_root)?,
            git_identity,
            exec_runner,
            cargo_registry_cache: None,
            sandbox: Default::default(),
            max_git_object_bytes: None,
            sandboxed_exec_timeout_bound: None,
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
                file_media: None,
            },
        })
    }

    /// Shares the process runner created during workspace tool composition.
    /// The production runner pins the executable on Linux; macOS retains only its canonical pathname.
    pub fn process_runner(&self) -> Option<ExecRunner> {
        self.executor
            .workspace_bound
            .as_ref()
            .map(SessionWorkspaceExecutors::process_runner)
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
