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
use signalbox_tools_github::{
    GITHUB_TOOL_NAMES, GitHubApiTransport, GitHubEgressPolicy, GitHubExecutor, GitHubTools,
    GitHubTransport,
};
use signalbox_tools_plan::{PLAN_TOOL_NAMES, PlanExecutor, PlanTools, SessionPlanPort};
use signalbox_tools_web::{
    ReqwestWebFetchTransport, WEB_FETCH_NAME, WebFetchEgressPolicy, WebFetchExecutor, WebFetchTool,
    WebFetchTransport,
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
    Writer,
    Credentials,
    HostTransport,
    GitHubTransportType,
    FileSystem: WorkspaceMutationFileSystem,
    ConversationPort,
    PlanPort,
> {
    web_fetch: WebFetchTool<Transport>,
    status: SessionStatusTool<Writer>,
    code_host: CodeHostTools<Credentials, HostTransport>,
    github: Option<GitHubTools<Credentials, GitHubTransportType>>,
    workspace_read: Option<WorkspaceReadTools<FileSystem>>,
    workspace_mutation: Option<WorkspaceMutationTools<FileSystem>>,
    conversations: Option<ConversationTools<ConversationPort>>,
    plan: PlanTools<PlanPort>,
    goal: Option<GoalDeclarationTool>,
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
    ConversationPort,
    PlanPort,
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
        ConversationPort,
        PlanPort,
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
        PinnedWorkspaceFileSystem,
        PostgresConversationIntrospection,
        SessionPlanRepository,
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
        let workspace = PinnedWorkspaceFileSystem::try_new(workspace_root)
            .map_err(|_| DaemonToolsConstructionError::WorkspaceRead)?;
        let workspace_read = WorkspaceReadTools::try_new(workspace.clone(), workspace_root)
            .map_err(|_| DaemonToolsConstructionError::WorkspaceRead)?;
        let workspace_mutation = WorkspaceMutationTools::try_new(workspace, workspace_root)
            .map_err(|_| DaemonToolsConstructionError::WorkspaceMutation)?;
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
                status,
                code_host,
                github: Some(github),
                workspace_read: Some(workspace_read),
                workspace_mutation: Some(workspace_mutation),
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
        credentials: FileCredentialAccess,
        code_host_transport: GitHubCodeHostTransport,
        web_fetch_egress_policy: WebFetchEgressPolicy,
    ) -> Result<Self, DaemonToolsConstructionError> {
        let web_fetch = WebFetchTool::try_new_production(web_fetch_egress_policy)
            .map_err(|_| DaemonToolsConstructionError::WebFetch)?;
        let status = SessionStatusTool::try_new_postgres(pool.clone())
            .map_err(|_| DaemonToolsConstructionError::SessionStatus)?;
        let goal = GoalDeclarationTool::try_new(pool.clone())
            .map_err(|_| DaemonToolsConstructionError::GoalDeclaration)?;
        let code_host = CodeHostTools::try_new(credentials, code_host_transport)
            .map_err(|_| DaemonToolsConstructionError::CodeHost)?;
        let plan = PlanTools::try_new(SessionPlanRepository::new(pool))
            .map_err(|_| DaemonToolsConstructionError::Plan)?;
        Self::try_new_with_tools(
            clock,
            ComposedToolFamilies {
                web_fetch,
                status,
                code_host,
                github: None,
                workspace_read: None,
                workspace_mutation: None,
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
    Writer,
    Credentials,
    HostTransport,
    GitHubTransportType,
    FileSystem,
    ConversationPort,
    PlanPort,
>
    DaemonTools<
        Clock,
        Transport,
        Writer,
        Credentials,
        HostTransport,
        GitHubTransportType,
        FileSystem,
        ConversationPort,
        PlanPort,
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
        conversation_port: ConversationPort,
        plan_port: PlanPort,
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
        let plan = PlanTools::try_new(plan_port).map_err(|_| DaemonToolsConstructionError::Plan)?;
        Self::try_new_with_tools(
            clock,
            ComposedToolFamilies {
                web_fetch,
                status,
                code_host,
                github: Some(github),
                workspace_read: Some(workspace_read),
                workspace_mutation: Some(workspace_mutation),
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
            Writer,
            Credentials,
            HostTransport,
            GitHubTransportType,
            FileSystem,
            ConversationPort,
            PlanPort,
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
        let (status_catalog, session_status) = status.into_parts();
        let (code_host_catalog, code_host) = code_host.into_parts();
        let github = github.map(GitHubTools::into_parts);
        let workspace_read = workspace_read.map(WorkspaceReadTools::into_parts);
        let workspace_mutation = workspace_mutation.map(WorkspaceMutationTools::into_parts);
        let conversations = conversations.map(ConversationTools::into_parts);
        let (plan_catalog, plan) = plan.into_parts();
        let goal = goal.map(GoalDeclarationTool::into_parts);
        let mut catalogs = vec![
            current_time_catalog,
            echo_catalog,
            web_fetch_catalog,
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
                session_status,
                code_host,
                github: github.map(|(_, executor)| executor),
                workspace_read: workspace_read.map(|(_, executor)| executor),
                workspace_mutation: workspace_mutation
                    .map(|(_, executor)| SharedToolExecutor::new(executor)),
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
            Writer,
            Credentials,
            HostTransport,
            GitHubTransportType,
            FileSystem,
            ConversationPort,
            PlanPort,
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
            Self::SessionStatus => "session_status_update tool construction failed",
            Self::CodeHost => "code-host tool suite construction failed",
            Self::GitHub => "GitHub pull-request tool suite construction failed",
            Self::WorkspaceRead => "workspace read tool suite construction failed",
            Self::WorkspaceMutation => "workspace mutation tool suite construction failed",
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
        for (name, posture) in postures {
            if !configured_composition_contains(&name, composition) {
                return Err(ConfiguredApprovalPostureError::UnknownTool { name });
            }
            if posture == ToolApprovalPosture::Delegated {
                return Err(ConfiguredApprovalPostureError::DelegatedJudgeUnavailable { name });
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
            if posture == ToolApprovalPosture::Delegated {
                return Err(ConfiguredApprovalPostureError::DelegatedJudgeUnavailable { name });
            }
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
                || CONVERSATION_TOOL_NAMES.contains(&name)
        }
    };
    name == CURRENT_TIME_NAME
        || name == ECHO_NAME
        || name == WEB_FETCH_NAME
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
    /// Delegated judging is not wired into this runtime yet.
    DelegatedJudgeUnavailable { name: ToolName },
}

impl ConfiguredApprovalPostureError {
    /// Borrows the configured tool name without exposing it to startup telemetry.
    pub const fn name(&self) -> &ToolName {
        match self {
            Self::UnknownTool { name } | Self::DelegatedJudgeUnavailable { name } => name,
        }
    }
}

impl fmt::Display for ConfiguredApprovalPostureError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::UnknownTool { .. } => "configured approval posture names an unknown tool",
            Self::DelegatedJudgeUnavailable { .. } => {
                "delegated approval posture requires judge wiring"
            }
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
    Writer,
    Credentials,
    HostTransport,
    GitHubTransportType,
    FileSystem: WorkspaceMutationFileSystem,
    ConversationPort,
    PlanPort,
> {
    current_time: CurrentTimeExecutor<Clock>,
    echo: EchoExecutor,
    web_fetch: WebFetchExecutor<Transport>,
    session_status: SessionStatusExecutor<Writer>,
    code_host: CodeHostExecutor<Credentials, HostTransport>,
    github: Option<GitHubExecutor<Credentials, GitHubTransportType>>,
    workspace_read: Option<WorkspaceReadExecutor<FileSystem>>,
    workspace_mutation: Option<SharedToolExecutor<WorkspaceMutationExecutor<FileSystem>>>,
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
    Writer,
    Credentials,
    HostTransport,
    GitHubTransportType,
    FileSystem,
    ConversationPort,
    PlanPort,
> ToolExecutor
    for DaemonToolExecutor<
        Clock,
        Transport,
        Writer,
        Credentials,
        HostTransport,
        GitHubTransportType,
        FileSystem,
        ConversationPort,
        PlanPort,
    >
where
    Clock: CurrentTimeClock,
    Transport: WebFetchTransport,
    Writer: SessionStatusWriter,
    Credentials: CredentialAccess,
    HostTransport: CodeHostTransport,
    GitHubTransportType: GitHubTransport,
    FileSystem: WorkspaceFileSystem + WorkspaceMutationFileSystem,
    ConversationPort: ConversationIntrospectionPort,
    PlanPort: SessionPlanPort,
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
    use std::{fmt, fs, time::SystemTime};

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
    fn composition_prevalidation_rejects_delegated_without_judge_wiring() {
        let echo = ToolName::try_new(String::from(ECHO_NAME)).expect("fixture name is valid");
        let rejected = DaemonToolCatalog::validate_approval_postures_for_composition(
            [(echo.clone(), ToolApprovalPosture::Delegated)],
            DaemonToolComposition::Base,
        )
        .expect_err("delegated posture fails before database construction");

        assert_eq!(
            rejected,
            ConfiguredApprovalPostureError::DelegatedJudgeUnavailable { name: echo }
        );
    }

    #[test]
    fn composed_catalog_rejects_delegated_posture_without_judge_wiring() {
        let (echo_catalog, _executor) = EchoTool::try_new()
            .expect("echo fixture compiles")
            .into_parts();
        let catalog = DaemonToolCatalog::try_new([echo_catalog])
            .expect("single-tool fixture has unique names");
        let echo = ToolName::try_new(String::from(ECHO_NAME)).expect("fixture name is valid");

        let rejected = catalog
            .with_approval_postures([(echo.clone(), ToolApprovalPosture::Delegated)])
            .expect_err("delegated posture fails closed without judge dispatch");

        assert_eq!(
            rejected,
            ConfiguredApprovalPostureError::DelegatedJudgeUnavailable { name: echo }
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
        let status =
            SessionStatusTool::try_new(OfflineWriter).expect("offline status tool compiles");
        let code_host = CodeHostTools::try_new(OfflineCredentials, OfflineCodeHostTransport)
            .expect("offline code-host tools compile");
        let (catalog, _executor) = DaemonTools::try_new_with_tools(
            || SystemTime::UNIX_EPOCH,
            ComposedToolFamilies {
                web_fetch,
                status,
                code_host,
                github: None::<GitHubTools<OfflineCredentials, OfflineGitHubTransport>>,
                workspace_read: None::<WorkspaceReadTools<LocalWorkspaceFileSystem>>,
                workspace_mutation: None::<WorkspaceMutationTools<LocalWorkspaceFileSystem>>,
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
            ]
        );
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
                signalbox_tools_plan::PLAN_READ_NAME,
                signalbox_tools_plan::PLAN_WRITE_NAME,
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
