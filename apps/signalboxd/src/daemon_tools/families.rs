use super::{
    DaemonToolsConstructionError,
    composed_identity::ComposedWorkspaceIdentity,
    session_workspace_roots::{SessionWorkspaceRoots, composed_root_identity},
    shared_executor::SharedToolExecutor,
};
use crate::{goal_mode::GoalDeclarationTool, session_delegation::DaemonSessionDelegationPort};
use signalbox_application::CompiledToolCatalog;
use signalbox_tools_basic::SessionStatusTool;
use signalbox_tools_code_host::CodeHostTools;
use signalbox_tools_conversations::ConversationTools;
use signalbox_tools_exec::{
    CargoDiagnosticsExecutor, CargoDiagnosticsTool, ExecExecutor, ProcessRunner,
    SandboxedCommandRunner, SandboxedExecTool, UnsandboxedCommandRunner, UnsandboxedExecTool,
};
use signalbox_tools_git::{GitIdentity, GitObjectFormat, LocalGitExecutor, LocalGitTools};
use signalbox_tools_github::GitHubTools;
use signalbox_tools_plan::PlanTools;
use signalbox_tools_sessions::SessionDelegationTools;
use signalbox_tools_web::{WebFetchTool, WebSearchTool};
use signalbox_tools_workspace::{
    WorkspaceFileSystem, WorkspaceMutationExecutor, WorkspaceMutationFileSystem,
    WorkspaceMutationTools, WorkspaceReadExecutor, WorkspaceReadTools,
};
use std::{
    fmt,
    path::{Path, PathBuf},
};

/// The six executors one workspace root binds.
pub(super) struct WorkspaceBoundExecutors<
    FileSystem: WorkspaceMutationFileSystem,
    ExecRunner: ProcessRunner,
> {
    pub(super) workspace_read: WorkspaceReadExecutor<FileSystem>,
    pub(super) workspace_mutation: SharedToolExecutor<WorkspaceMutationExecutor<FileSystem>>,
    pub(super) local_git: SharedToolExecutor<LocalGitExecutor<FileSystem>>,
    pub(super) sandboxed_exec: ExecExecutor<SandboxedCommandRunner<ExecRunner>>,
    pub(super) unsandboxed_exec: ExecExecutor<UnsandboxedCommandRunner<ExecRunner>>,
    pub(super) cargo_diagnostics: CargoDiagnosticsExecutor<ExecRunner>,
    pub(super) git_object_format: GitObjectFormat,
    pub(super) workspace_identity: ComposedWorkspaceIdentity,
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
pub(super) struct WorkspaceBoundFamilies<
    FileSystem: WorkspaceMutationFileSystem,
    ExecRunner: ProcessRunner,
> {
    pub(super) catalogs: [CompiledToolCatalog; 6],
    pub(super) executors: WorkspaceBoundExecutors<FileSystem, ExecRunner>,
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
    pub(super) fn try_new(
        filesystem: FileSystem,
        root: &Path,
        git_identity: GitIdentity,
        exec_runner: ExecRunner,
        cargo_registry_cache: Option<&Path>,
        sandbox: &signalbox_tools_exec::SandboxConfiguration,
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
                    root_count = 1,
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
        let sandboxed_exec = sandboxed_exec.with_sandbox_configuration(sandbox.clone());
        let unsandboxed_exec = UnsandboxedExecTool::try_new(exec_runner.clone(), root)
            .map_err(|_| DaemonToolsConstructionError::Exec)?;
        let cargo_diagnostics = match cargo_registry_cache {
            Some(cache) => {
                CargoDiagnosticsTool::try_new_with_cargo_registry(exec_runner, root, cache)
            }
            None => CargoDiagnosticsTool::try_new(exec_runner, root),
        }
        .map_err(|_| DaemonToolsConstructionError::Exec)?;
        let cargo_diagnostics = cargo_diagnostics.with_sandbox_configuration(sandbox.clone());
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

pub(super) struct ComposedToolFamilies<
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
    pub(super) web_fetch: WebFetchTool<Transport>,
    pub(super) web_search: WebSearchTool<Credentials, SearchTransport>,
    pub(super) status: SessionStatusTool<Writer>,
    pub(super) code_host: CodeHostTools<Credentials, HostTransport>,
    pub(super) github: Option<GitHubTools<Credentials, GitHubTransportType>>,
    pub(super) workspace_bound: Option<ConfiguredWorkspaceComposition<FileSystem, ExecRunner>>,
    pub(super) conversations: Option<ConversationTools<ConversationPort>>,
    pub(super) plan: PlanTools<PlanPort>,
    pub(super) delegation: SessionDelegationTools<DaemonSessionDelegationPort>,
    pub(super) goal: Option<GoalDeclarationTool>,
}

/// The configured root's own families beside the derivation later sessions use.
pub(super) struct ConfiguredWorkspaceComposition<
    FileSystem: WorkspaceMutationFileSystem,
    ExecRunner: ProcessRunner,
> {
    pub(super) families: WorkspaceBoundFamilies<FileSystem, ExecRunner>,
    pub(super) roots: SessionWorkspaceRoots,
    pub(super) git_identity: GitIdentity,
    pub(super) exec_runner: ExecRunner,
    pub(super) cargo_registry_cache: Option<PathBuf>,
    pub(super) sandbox: signalbox_tools_exec::SandboxConfiguration,
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
