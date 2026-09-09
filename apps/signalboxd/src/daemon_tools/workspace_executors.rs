use super::{
    DaemonToolsConstructionError,
    composed_identity::ComposedWorkspaceIdentity,
    executor::DaemonToolExecutorError,
    families::{ConfiguredWorkspaceComposition, WorkspaceBoundExecutors, WorkspaceBoundFamilies},
    pinned_file_system::PinFurtherWorkspaceRoot,
    retained_workspaces::SessionWorkspaceState,
    session_workspace_roots::{
        RecordedSessionBinding, SessionRootDecision, SessionWorkspaceRoot, SessionWorkspaceRoots,
        WorkspaceInstructionRootAuthority, WorkspaceInstructionRootFuture,
        WorkspaceInstructionRootResolutionError, a_derived_binding_exists,
        a_derived_binding_shares_the_configured_root, another_session_bound,
        composition_aliases_its_own_parent, decide_session_root,
        parent_aliases_the_configured_root, probe_is_stale,
        shares_a_directory_with_the_configured_root,
    },
    workspace_failure::{SessionWorkspaceFailure, SessionWorkspaceFailureDetails},
};
use signalbox_application::{
    CorrelatedToolExecutorEvidence, ToolExecutionInvocation, ToolExecutor, ToolExecutorEvidence,
};
use signalbox_domain::SessionId;
use signalbox_tools_exec::{
    CARGO_DIAGNOSTICS_NAME, ProcessRunner, SANDBOXED_EXEC_NAME, UNSANDBOXED_EXEC_NAME,
};
use signalbox_tools_git::{GitIdentity, LOCAL_GIT_TOOL_NAMES};
use signalbox_tools_workspace::{
    WORKSPACE_MUTATION_TOOL_NAMES, WORKSPACE_READ_TOOL_NAMES, WorkspaceFileSystem,
    WorkspaceMutationFileSystem,
};
use std::{fmt, path::PathBuf, sync::Arc};
use tokio::sync::Mutex;

/// Resolves the workspace-bound executors one session's tool calls dispatch to.
///
/// The configured root's own set is composed at startup and shared by every
/// session whose derived root is absent, so an unprovisioned deployment keeps
/// exactly the composition, descriptors, and failure timing it had before.
pub(super) struct SessionWorkspaceExecutors<
    FileSystem: WorkspaceMutationFileSystem,
    ExecRunner: ProcessRunner,
> {
    roots: SessionWorkspaceRoots,
    pub(super) repository_watch: Option<crate::repo_watch_runtime::RepositoryWatchRuntime>,
    git_identity: GitIdentity,
    exec_runner: ExecRunner,
    cargo_registry_cache: Option<PathBuf>,
    sandbox: signalbox_tools_exec::SandboxConfiguration,
    configured: WorkspaceBoundExecutors<FileSystem, ExecRunner>,
    failure_details: SessionWorkspaceFailureDetails,
    state: Arc<Mutex<SessionWorkspaceState<WorkspaceBoundExecutors<FileSystem, ExecRunner>>>>,
}

impl<FileSystem: WorkspaceMutationFileSystem, ExecRunner: ProcessRunner> Clone
    for SessionWorkspaceExecutors<FileSystem, ExecRunner>
{
    fn clone(&self) -> Self {
        Self {
            roots: self.roots.clone(),
            repository_watch: self.repository_watch.clone(),
            git_identity: self.git_identity.clone(),
            exec_runner: self.exec_runner.clone(),
            cargo_registry_cache: self.cargo_registry_cache.clone(),
            sandbox: self.sandbox.clone(),
            configured: self.configured.clone(),
            failure_details: self.failure_details.clone(),
            state: Arc::clone(&self.state),
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
    pub(super) fn try_new(
        composition: ConfiguredWorkspaceComposition<FileSystem, ExecRunner>,
    ) -> Result<Self, DaemonToolsConstructionError> {
        let ConfiguredWorkspaceComposition {
            families,
            roots,
            git_identity,
            exec_runner,
            cargo_registry_cache,
            sandbox,
        } = composition;
        let failure_details = SessionWorkspaceFailureDetails::try_new()?;
        Ok(Self {
            roots,
            repository_watch: None,
            git_identity,
            exec_runner,
            cargo_registry_cache,
            sandbox,
            configured: families.executors,
            failure_details,
            state: Arc::new(Mutex::new(SessionWorkspaceState::new())),
        })
    }

    pub(super) fn process_runner(&self) -> ExecRunner {
        self.exec_runner.clone()
    }

    async fn resolve_workspace_instruction_root(
        &mut self,
        session: SessionId,
    ) -> Result<PathBuf, SessionWorkspaceFailure> {
        let executors = self.resolve(session).await?;
        let path = match self.state.lock().await.bindings.get(&session) {
            Some(RecordedSessionBinding::ConfiguredRoot) => Ok(self.roots.configured().to_owned()),
            Some(RecordedSessionBinding::DerivedRoot { .. }) => {
                Ok(self.roots.derived_path(session))
            }
            None => Err(SessionWorkspaceFailure::UnresolvableRoot),
        }?;
        let standing = ComposedWorkspaceIdentity::capture(&path)
            .map_err(|_| SessionWorkspaceFailure::ReplacedRootIdentity)?;
        if standing != executors.workspace_identity {
            return Err(SessionWorkspaceFailure::ReplacedRootIdentity);
        }
        Ok(path)
    }

    async fn resolve(
        &mut self,
        session: SessionId,
    ) -> Result<WorkspaceBoundExecutors<FileSystem, ExecRunner>, SessionWorkspaceFailure> {
        // The derivation is probed before the retained executors are consulted,
        // not after. A retained set is a set of descriptors pinned to one
        // directory, and returning it without asking what the pathname names
        // now would let a session keep reading and writing a tree the
        // deployment has already removed or replaced.
        let probed = self.roots.resolve(session);
        let mut state = self.state.lock().await;
        let recorded = state.bindings.get(&session).copied();
        // The probe above was taken before the lock, so a concurrent first
        // request for this session may have bound a derived root in between.
        // Retaking it under the lock is what distinguishes that from the
        // directory having been removed, which reads identically and must still
        // fail closed.
        let derived = if probe_is_stale(recorded, &probed) {
            self.roots.resolve(session)
        } else {
            probed
        };
        // The pathname to compose against, and the directory the derivation
        // walked through to reach it.
        let (path, parent) = match decide_session_root(recorded, &derived) {
            SessionRootDecision::ConfiguredRoot => {
                // Admission is not a durable answer for this branch either.
                // The configured composition is never re-resolved, so what its
                // pathname names can change after startup — its `.git`
                // bind-mounted from a derived session's workspace, say — and
                // returning the configured executors on the strength of the
                // startup comparison alone would let this session reach that
                // workspace while the session that bound it dispatches under a
                // separate serialization domain. The derived branch remakes this
                // comparison on every dispatch; remaking it only there would
                // protect only the requests that take that branch.
                if a_derived_binding_exists(&state.bindings, session) {
                    let standing_configured =
                        ComposedWorkspaceIdentity::capture(self.roots.configured())
                            .map_err(|_| SessionWorkspaceFailure::UnverifiableConfiguredRoot)?;
                    if a_derived_binding_shares_the_configured_root(
                        &state.bindings,
                        session,
                        self.configured.workspace_identity,
                        standing_configured,
                    ) {
                        return Err(SessionWorkspaceFailure::SharedRootIdentity);
                    }
                }
                // Reachable only with no record or a recorded configured
                // binding, so the entry below can only read as configured. The
                // derived arm returns no retained set: nothing on this path
                // revalidated one, and a set returned unrevalidated is the
                // defect the revalidation exists to prevent.
                return match *state
                    .bindings
                    .entry(session)
                    .or_insert(RecordedSessionBinding::ConfiguredRoot)
                {
                    RecordedSessionBinding::ConfiguredRoot => Ok(self.configured.clone()),
                    RecordedSessionBinding::DerivedRoot { .. } => {
                        Err(SessionWorkspaceFailure::UnresolvableRoot)
                    }
                };
            }
            SessionRootDecision::Unresolvable => {
                return Err(SessionWorkspaceFailure::UnresolvableRoot);
            }
            SessionRootDecision::ComposeDerived => {
                let SessionWorkspaceRoot::Derived { path, parent } = &derived else {
                    return Err(SessionWorkspaceFailure::UnresolvableRoot);
                };
                // A recorded binding names both directories, and a retained
                // composition is returned only once both still stand at the
                // pathname. Revalidating the worktree root alone would hand
                // back descriptors whose Git executor is pinned to an
                // administration directory the pathname no longer names, which
                // is provisioning that replaces only a workspace's `.git`. A
                // pathname whose pair can no longer be captured at all — a
                // removed `.git`, say — fails for the same reason.
                if let Some(bound) = recorded.and_then(RecordedSessionBinding::derived_identity) {
                    let standing = ComposedWorkspaceIdentity::capture(path)
                        .map_err(|_| SessionWorkspaceFailure::ReplacedRootIdentity)?;
                    if standing != bound {
                        return Err(SessionWorkspaceFailure::ReplacedRootIdentity);
                    }
                    // The pair can stand unchanged while the directory walked
                    // through to reach it does not: a parent renamed away and
                    // replaced, with this session's directory moved under the
                    // replacement, leaves both bound directories intact at the
                    // same pathname. The component the classification accepted
                    // is therefore revalidated beside the pair it leads to.
                    if recorded.and_then(RecordedSessionBinding::derived_parent) != Some(*parent) {
                        return Err(SessionWorkspaceFailure::ReplacedRootIdentity);
                    }
                    // The pair above was captured by walking the pathname
                    // again, and that walk went through the parent this
                    // request's probe classified before it. Classification and
                    // walk are separate instants, so the component is re-read
                    // after the walk exactly as a new composition re-reads it:
                    // a parent replaced by a symlink in between is followed by
                    // the capture, and a child moved under the replacement
                    // leaves both bound identities standing, so the pair alone
                    // cannot show it.
                    if self.roots.standing_parent() != Some(*parent) {
                        return Err(SessionWorkspaceFailure::ReplacedRootIdentity);
                    }
                    // Admission is not a durable answer. The configured
                    // composition is never re-resolved, so what its pathname
                    // names can change after this session was admitted — a
                    // `.git` bind-mounted over this session's own, say — and a
                    // retained set returned on the strength of the comparison
                    // made at admission would leave both reaching one tree
                    // under separate serialization domains. The comparison is
                    // therefore remade on every dispatch, before the retained
                    // set is consulted and before a recomposition begins.
                    let standing_configured =
                        ComposedWorkspaceIdentity::capture(self.roots.configured())
                            .map_err(|_| SessionWorkspaceFailure::UnverifiableConfiguredRoot)?;
                    if shares_a_directory_with_the_configured_root(
                        bound,
                        self.configured.workspace_identity,
                        standing_configured,
                    ) {
                        return Err(SessionWorkspaceFailure::SharedRootIdentity);
                    }
                    // The pair can be disjoint from the configured pair while
                    // the directory walked through to reach it is one of them,
                    // which nests this whole workspace inside the configured
                    // root.
                    if parent_aliases_the_configured_root(
                        *parent,
                        self.configured.workspace_identity,
                        standing_configured,
                    ) {
                        return Err(SessionWorkspaceFailure::SharedRootIdentity);
                    }
                    // The bound pair can equal the directory it is reached
                    // through, which makes this session's workspace the one
                    // holding every sibling session's root.
                    if composition_aliases_its_own_parent(bound, *parent) {
                        return Err(SessionWorkspaceFailure::SharedRootIdentity);
                    }
                }
                if let Some(retained) = state.retained.get(session) {
                    return Ok(retained);
                }
                (path.clone(), *parent)
            }
        };
        drop(state);
        let filesystem = FileSystem::pin_further_root(&path).map_err(|_| {
            SessionWorkspaceFailure::Composition(DaemonToolsConstructionError::WorkspaceRead)
        })?;
        let families = WorkspaceBoundFamilies::try_new(
            filesystem,
            &path,
            self.git_identity.clone(),
            self.exec_runner.clone(),
            self.cargo_registry_cache.as_deref(),
            &self.sandbox,
        )
        .map_err(SessionWorkspaceFailure::Composition)?;
        // Every family above resolved the derived pathname independently, and
        // each walked through the parent to do it. The parent's no-follow
        // classification happened once, before any of them ran, so it is
        // remade here: a parent renamed away and replaced by a symlink during
        // composition is followed by every one of those resolutions, and the
        // bound pair alone cannot show it, since ancestry is not equality.
        if self.roots.standing_parent() != Some(parent) {
            return Err(SessionWorkspaceFailure::ReplacedRootIdentity);
        }
        if families.executors.git_object_format != self.configured.git_object_format {
            return Err(SessionWorkspaceFailure::ObjectFormatDisagreement);
        }
        let composed = families.executors.workspace_identity;
        // Two pathnames can name one workspace — a bind mount, a derived path
        // exposing the configured root, or two roots over one repository — and
        // each would pass composition on its own. The isolation this derivation
        // exists to establish is a property of the directories, not of the
        // pathname, so it is checked against what every other binding pinned.
        let standing_configured = ComposedWorkspaceIdentity::capture(self.roots.configured())
            .map_err(|_| SessionWorkspaceFailure::UnverifiableConfiguredRoot)?;
        if shares_a_directory_with_the_configured_root(
            composed,
            self.configured.workspace_identity,
            standing_configured,
        ) {
            return Err(SessionWorkspaceFailure::SharedRootIdentity);
        }
        // The composed pair can be disjoint from the configured pair while the
        // directory every family walked through to reach it is one of them —
        // `<name>.sessions` bind-mounted onto the configured root — which nests
        // this workspace inside the tree the derivation exists to leave.
        if parent_aliases_the_configured_root(
            parent,
            self.configured.workspace_identity,
            standing_configured,
        ) {
            return Err(SessionWorkspaceFailure::SharedRootIdentity);
        }
        // The composed pair can be disjoint from the configured composition and
        // from every other session's, and still be the directory this pathname
        // was reached through — a session identifier directory bind-mounted
        // onto `<name>.sessions` itself — which makes this workspace the one
        // holding every sibling session's root.
        if composition_aliases_its_own_parent(composed, parent) {
            return Err(SessionWorkspaceFailure::SharedRootIdentity);
        }
        let mut state = self.state.lock().await;
        // Every other derived binding revalidates its own pair before its next
        // request dispatches, so a derived workspace whose directories changed
        // fails that session closed rather than being reachable beside this
        // one; the pairs recorded here are the ones those sessions can still
        // use.
        if another_session_bound(&state.bindings, session, composed) {
            return Err(SessionWorkspaceFailure::SharedRootIdentity);
        }
        match *state
            .bindings
            .entry(session)
            .or_insert(RecordedSessionBinding::DerivedRoot {
                identity: composed,
                parent,
            }) {
            // A concurrent first request bound the configured root; its record
            // wins, and this composition is released rather than retained.
            RecordedSessionBinding::ConfiguredRoot => return Ok(self.configured.clone()),
            // The pathname now names a different directory than the one this
            // session bound, or reaches it through a different one, so the
            // session is not resuming its own workspace.
            RecordedSessionBinding::DerivedRoot {
                identity,
                parent: bound_parent,
            } if identity != composed || bound_parent != parent => {
                return Err(SessionWorkspaceFailure::ReplacedRootIdentity);
            }
            RecordedSessionBinding::DerivedRoot { .. } => {}
        }
        Ok(state.retained.retain(session, families.executors))
    }

    /// Dispatches one workspace-root-bound request to the requesting session's
    /// own executors.
    ///
    /// An unresolvable session workspace closes the attempt as a known tool
    /// failure whose sanitized detail names the closed reason — the model, the
    /// transcript, and both clients see it. It is never silently redirected to
    /// another session's root.
    pub(super) async fn execute(
        &mut self,
        invocation: ToolExecutionInvocation,
    ) -> Result<CorrelatedToolExecutorEvidence, DaemonToolExecutorError> {
        let session = invocation.correlation().session();
        if invocation.request().name().as_str() == signalbox_tools_git::GIT_PUSH_CONFIGURED_NAME {
            let authority = match &self.repository_watch {
                Some(watch) => watch
                    .git_push_authority(session)
                    .await
                    .map_err(|_| DaemonToolExecutorError::pre_dispatch())?,
                None => None,
            };
            let Some((repository, branch, commit)) = authority else {
                return Ok(invocation.bind(ToolExecutorEvidence::KnownFailed {
                    detail: signalbox_domain::ToolExecutionErrorDetail::try_new(
                        "configured Git push is unavailable for this session".to_owned(),
                    )
                    .ok(),
                }));
            };
            let root = self
                .resolve_workspace_instruction_root(session)
                .await
                .map_err(|_| DaemonToolExecutorError::pre_dispatch())?;
            if root != self.roots.derived_path(session) {
                return Err(DaemonToolExecutorError::pre_dispatch());
            }
            let filesystem = FileSystem::pin_further_root(&root)
                .map_err(|_| DaemonToolExecutorError::pre_dispatch())?;
            let mut executor = WorkspaceBoundFamilies::git_push(
                &root,
                &repository,
                branch,
                commit,
                self.exec_runner.clone(),
                &filesystem,
            )
            .map_err(|_| DaemonToolExecutorError::pre_dispatch())?;
            self.resolve_workspace_instruction_root(session)
                .await
                .map_err(|_| DaemonToolExecutorError::pre_dispatch())?;
            return executor
                .execute(invocation)
                .await
                .map_err(|error| DaemonToolExecutorError::from_error(&error));
        }
        let mut executors = match self.resolve(session).await {
            Ok(executors) => executors,
            Err(failure) => {
                // No event is emitted here. The tool loop already emits one
                // failed-attempt event at its single admission site, and the
                // closed reason travels in the durable result below rather than
                // in a second operator event beside it.
                let _ = failure.discriminant();
                return Ok(invocation.bind(ToolExecutorEvidence::KnownFailed {
                    detail: Some(self.failure_details.detail(failure)),
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

impl<FileSystem, ExecRunner> WorkspaceInstructionRootAuthority
    for SessionWorkspaceExecutors<FileSystem, ExecRunner>
where
    FileSystem: WorkspaceFileSystem
        + WorkspaceMutationFileSystem
        + PinFurtherWorkspaceRoot
        + Send
        + Sync
        + 'static,
    ExecRunner: ProcessRunner + Send + Sync + 'static,
{
    fn resolve(&self, session: SessionId) -> WorkspaceInstructionRootFuture<'_> {
        let mut executors = self.clone();
        Box::pin(async move {
            executors
                .resolve_workspace_instruction_root(session)
                .await
                .map_err(|_| WorkspaceInstructionRootResolutionError)
        })
    }
}
