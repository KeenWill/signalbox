//! Process-lifetime compiled daemon tool catalog and executor dispatch.
//!
//! The catalog is one process-lifetime immutable compiled value; the executors
//! a workspace root binds are per session, resolved through
//! [`SessionWorkspaceRoots`]. See `docs/spec/tool-loop.md` and
//! `docs/spec/git-authority-threat-model.md`.

use std::{
    collections::BTreeMap,
    error::Error,
    fmt, fs,
    future::Future,
    io,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    pin::Pin,
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
    PinnedRepositoryDirectories,
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
    WorkspaceRootIdentity,
};
use sqlx::PgPool;
use tokio::sync::Mutex;

use crate::{
    FileCredentialAccess, PostgresConversationIntrospection,
    blob_tools::{BLOB_METADATA_NAME, BLOB_READ_NAME, BLOB_TOOL_NAMES, BlobToolExecutor},
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

    fn read_file_range(
        &self,
        root: &WorkspaceRoot,
        path: &Path,
        offset: u64,
        max_bytes: usize,
    ) -> Result<WorkspaceFileBytes, WorkspaceResolveError> {
        self.local.read_file_range(root, path, offset, max_bytes)
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

/// Administration directory the Git family requires immediately inside a root.
const GIT_ADMINISTRATION_DIRECTORY: &str = ".git";

const SESSION_WORKSPACE_COMPOSITION_DETAIL: &str = "session workspace could not be composed";

const SESSION_WORKSPACE_OBJECT_FORMAT_DETAIL: &str =
    "session workspace repository uses another object format";

const SESSION_WORKSPACE_UNRESOLVABLE_DETAIL: &str = "session workspace root is unresolvable";

const SESSION_WORKSPACE_SHARED_DETAIL: &str =
    "session workspace root is shared with another session";

const SESSION_WORKSPACE_REPLACED_DETAIL: &str =
    "session workspace root changed since this session bound it";

const SESSION_WORKSPACE_UNVERIFIABLE_CONFIGURED_DETAIL: &str =
    "configured workspace root could not be revalidated";

/// Derives each session's workspace root from the configured root by a fixed
/// formula.
///
/// A session names no path: the derivation takes only the configured root and
/// the session's own identity, so the set of roots the daemon can ever open is
/// determined by deployment configuration alone.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionWorkspaceRoots {
    configured: PathBuf,
    derived_parent: PathBuf,
}

type WorkspaceInstructionRootFuture<'a> = Pin<
    Box<dyn Future<Output = Result<PathBuf, WorkspaceInstructionRootResolutionError>> + Send + 'a>,
>;

trait WorkspaceInstructionRootAuthority: Send + Sync {
    fn resolve(&self, session: SessionId) -> WorkspaceInstructionRootFuture<'_>;
}

/// Cloneable access to the workspace-binding authority used by daemon tools.
///
/// Instruction discovery uses this handle so it cannot independently choose a
/// different configured-versus-derived root for a session whose binding is
/// already sticky.
#[derive(Clone)]
pub struct WorkspaceInstructionRootResolver {
    authority: Arc<dyn WorkspaceInstructionRootAuthority>,
}

impl fmt::Debug for WorkspaceInstructionRootResolver {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkspaceInstructionRootResolver")
            .finish_non_exhaustive()
    }
}

impl WorkspaceInstructionRootResolver {
    fn new<FileSystem, ExecRunner>(
        executors: SessionWorkspaceExecutors<FileSystem, ExecRunner>,
    ) -> Self
    where
        FileSystem: WorkspaceFileSystem
            + WorkspaceMutationFileSystem
            + PinFurtherWorkspaceRoot
            + Send
            + Sync
            + 'static,
        ExecRunner: ProcessRunner + Send + Sync + 'static,
    {
        Self {
            authority: Arc::new(executors),
        }
    }

    pub(crate) async fn resolve(
        &self,
        session: SessionId,
    ) -> Result<PathBuf, WorkspaceInstructionRootResolutionError> {
        self.authority.resolve(session).await
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct WorkspaceInstructionRootResolutionError;

impl SessionWorkspaceRoots {
    /// Fixes the derivation against one configured workspace root.
    ///
    /// A configured root the formula cannot be applied to is rejected here
    /// rather than carried as a derivation that answers "unprovisioned" for
    /// every session. `/srv/workspace/child/..` is absolute, is accepted by the
    /// configuration surface, and can name a valid worktree, but it has no
    /// lexical final component to append the suffix to. Treating that as an
    /// unprovisioned deployment would silently bind every session to the
    /// configured composition — the shared root this derivation exists to
    /// replace — and no directory provisioned by the documented formula would
    /// ever be considered.
    pub fn try_new(configured: &Path) -> Result<Self, DaemonToolsConstructionError> {
        let (parent, name) = configured
            .parent()
            .zip(configured.file_name())
            .ok_or(DaemonToolsConstructionError::WorkspaceRootUnderivable)?;
        let mut directory = name.to_owned();
        directory.push(SESSION_WORKSPACE_DIRECTORY_SUFFIX);
        Ok(Self {
            configured: configured.to_owned(),
            derived_parent: parent.join(directory),
        })
    }

    /// Returns the configured root every derivation is taken from.
    #[must_use]
    pub fn configured(&self) -> &Path {
        &self.configured
    }

    /// Returns the path the formula assigns one session, before asking whether
    /// a directory exists there.
    #[must_use]
    pub fn derived_path(&self, session: SessionId) -> PathBuf {
        self.derived_parent.join(session.into_uuid().to_string())
    }

    /// Returns what the derivation currently finds for one session.
    ///
    /// The probe classifies rather than tests: `Path::is_dir` collapses a
    /// denied traversal or an I/O error into the same answer as an absent
    /// directory, and binding the configured root on that answer would send a
    /// provisioned session's writes to a tree it was not provisioned with. Only
    /// a reported absence is unprovisioned. A present non-directory — including
    /// a symlink, which the pinned no-follow open would refuse anyway — is a
    /// misprovisioned session rather than an unprovisioned one.
    ///
    /// The parent is classified the same way and for the same reason. It is the
    /// one intermediate component this derivation introduces, and every later
    /// no-follow open declines to follow only the component it names, so a
    /// symlink standing at the parent is followed by all of them. Its identity
    /// travels with the answer because classifying it once is a statement about
    /// one instant, and the pathname is walked again by every family that
    /// composes and by every request that revalidates.
    #[must_use]
    pub fn resolve(&self, session: SessionId) -> SessionWorkspaceRoot {
        let path = self.derived_path(session);
        // A symlink at `<name>.sessions` — pointing inside the configured root,
        // say — would place every derived root under a tree every session still
        // bound to the configured root can read, write, and execute, which is
        // the containment the sibling derivation exists to establish. Resolving
        // the pathname below would follow it, and the no-follow opens after it
        // protect only the session's own final component.
        let parent = match fs::symlink_metadata(&self.derived_parent) {
            Ok(metadata) if metadata.is_dir() => ComposedRootIdentity::from_metadata(&metadata),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return SessionWorkspaceRoot::ConfiguredRoot;
            }
            Ok(_) | Err(_) => return SessionWorkspaceRoot::Unresolvable,
        };
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_dir() => SessionWorkspaceRoot::Derived { path, parent },
            Ok(_) => SessionWorkspaceRoot::Unresolvable,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                SessionWorkspaceRoot::ConfiguredRoot
            }
            Err(_) => SessionWorkspaceRoot::Unresolvable,
        }
    }

    /// Captures the identity the derived parent's pathname names right now,
    /// declining to follow a symlink standing there.
    ///
    /// Used to revalidate the component the probe classified, since a caller
    /// walks the pathname again after the probe and the classification says
    /// nothing about the instants after it.
    fn standing_parent(&self) -> Option<ComposedRootIdentity> {
        match fs::symlink_metadata(&self.derived_parent) {
            Ok(metadata) if metadata.is_dir() => {
                Some(ComposedRootIdentity::from_metadata(&metadata))
            }
            Ok(_) | Err(_) => None,
        }
    }
}

/// What the derivation currently finds at one session's derived path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionWorkspaceRoot {
    /// A directory exists at the session's derived path and binds it alone.
    ///
    /// The bound pair itself is not carried: which directories a session bound
    /// is a property of both the worktree and the administration directory
    /// inside it, and a caller comparing a binding captures that pair rather
    /// than the one directory a classification needed to stat. The parent is
    /// carried, because it is walked through rather than bound, and nothing
    /// downstream can recover which directory the classification accepted.
    Derived {
        /// The derived absolute path.
        path: PathBuf,
        /// Identity of the parent directory the classification accepted, so a
        /// caller can tell that the component it walks through is still the one
        /// that was classified.
        parent: ComposedRootIdentity,
    },
    /// Nothing exists at the session's derived path, so an unbound session
    /// binds the configured root that every session bound before this
    /// derivation.
    ConfiguredRoot,
    /// Something exists at the session's derived path that is not a directory,
    /// or the path could not be classified at all.
    Unresolvable,
}

/// Which root a session bound the first time it used a workspace-bound tool.
///
/// Recorded so the binding is sticky for the process's lifetime: a session that
/// bound a derived root is never returned to the configured root by that
/// directory's later removal, and a session that bound the configured root is
/// never moved off it by a directory appearing mid-session. The record holds one
/// identity and one discriminant per session that used a workspace-bound tool —
/// no descriptor, and no path, because the path is re-derivable — so it is kept
/// outside the descriptor-bounded retained set and is never evicted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RecordedSessionBinding {
    /// The session bound the configured root.
    ConfiguredRoot,
    /// The session bound its own derived root, whose filesystem identity is
    /// retained so a directory replaced at the same pathname is refused rather
    /// than composed as though it were the same workspace.
    DerivedRoot {
        /// Identities of the worktree and administration directories this
        /// session bound.
        identity: ComposedWorkspaceIdentity,
        /// Identity of the directory the derivation walked through to reach
        /// them. Recorded apart from the bound pair because it is traversed
        /// rather than bound: two sessions legitimately share one parent, so it
        /// is never a collision, but a different directory standing there means
        /// the pathname no longer leads where it led when this session bound.
        parent: ComposedRootIdentity,
    },
}

impl RecordedSessionBinding {
    /// Returns the identity this binding pinned, if it pinned a derived root.
    const fn derived_identity(self) -> Option<ComposedWorkspaceIdentity> {
        match self {
            Self::ConfiguredRoot => None,
            Self::DerivedRoot { identity, .. } => Some(identity),
        }
    }

    /// Returns the parent this binding walked through, if it pinned a derived
    /// root.
    const fn derived_parent(self) -> Option<ComposedRootIdentity> {
        match self {
            Self::ConfiguredRoot => None,
            Self::DerivedRoot { parent, .. } => Some(parent),
        }
    }
}

/// Whether a probe taken before the state lock has to be retaken under it.
///
/// A first request can observe an absent directory, be descheduled before it
/// takes the lock, and resume after a concurrent request for the same session
/// has provisioned nothing but *observed* the directory, composed it, and
/// recorded a derived binding. Failing the resuming request on that stale
/// observation would make two concurrent first requests diverge where the
/// contract has them converge on the first record written. A genuinely removed
/// directory reads the same way, so the answer is to look again rather than to
/// guess: the retaken probe distinguishes them.
const fn probe_is_stale(
    recorded: Option<RecordedSessionBinding>,
    derived: &SessionWorkspaceRoot,
) -> bool {
    matches!(
        (recorded, derived),
        (
            Some(RecordedSessionBinding::DerivedRoot { .. }),
            SessionWorkspaceRoot::ConfiguredRoot
        )
    )
}

/// What a session's next workspace-bound request binds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SessionRootDecision {
    /// Bind the configured root's own composition.
    ConfiguredRoot,
    /// Compose against the derived root the derivation found.
    ComposeDerived,
    /// Fail closed rather than bind a root this session was not provisioned
    /// with.
    Unresolvable,
}

/// Decides what a session binds from its recorded binding and what the
/// derivation currently finds.
///
/// A recorded configured binding answers for every classification, including a
/// misprovisioned one. Such a session never opens the derived pathname, so an
/// entry appearing there — directory, file, or symlink alike — is unreachable
/// by it and cannot change the tree it already uses. Failing it closed would
/// deny it for the rest of the process's lifetime over a condition it cannot
/// act on and that grants it nothing, and would do so only for the botched
/// spelling of provisioning that arrived too late while a correct one arriving
/// equally late is ignored. The misprovisioning classification decides the
/// sessions whose binding is still open, which is where it decides anything.
const fn decide_session_root(
    recorded: Option<RecordedSessionBinding>,
    derived: &SessionWorkspaceRoot,
) -> SessionRootDecision {
    match (recorded, derived) {
        (None, SessionWorkspaceRoot::ConfiguredRoot)
        | (Some(RecordedSessionBinding::ConfiguredRoot), _) => SessionRootDecision::ConfiguredRoot,
        (
            None | Some(RecordedSessionBinding::DerivedRoot { .. }),
            SessionWorkspaceRoot::Derived { .. },
        ) => SessionRootDecision::ComposeDerived,
        (None, SessionWorkspaceRoot::Unresolvable)
        | (
            Some(RecordedSessionBinding::DerivedRoot { .. }),
            SessionWorkspaceRoot::ConfiguredRoot | SessionWorkspaceRoot::Unresolvable,
        ) => SessionRootDecision::Unresolvable,
    }
}

/// Whether a composition shares a directory with the configured composition.
///
/// The configured composition is built once at startup and is the one binding
/// no later request re-resolves, so `pinned` names the directories it held then
/// while `standing` names the ones its pathname resolves to now. Its worktree
/// descriptor is pinned, but its mutation and execution tools reach `.git`
/// through that descriptor by name, so a `.git` renamed and recreated under the
/// configured root is reachable from it while `pinned` still names the old one.
/// Both pairs are therefore refused.
///
/// `standing` is not optional. A caller that could not capture it knows less
/// than this comparison needs, not more: the configured adapter still holds its
/// root descriptor and still reaches whatever `.git` stands under it, so a
/// failed capture is a reason to refuse rather than a reason to compare against
/// `pinned` alone. Making the unknown unrepresentable here is what stops that
/// degradation being reintroduced at a call site.
const fn shares_a_directory_with_the_configured_root(
    composed: ComposedWorkspaceIdentity,
    pinned: ComposedWorkspaceIdentity,
    standing: ComposedWorkspaceIdentity,
) -> bool {
    composed.shares_a_directory_with(pinned) || composed.shares_a_directory_with(standing)
}

/// Whether a session other than `session` already bound the directory a
/// composition just found.
///
/// Asked of the directory rather than of the pathname, because two pathnames
/// can name one directory and each would compose successfully on its own.
fn another_session_bound(
    bindings: &BTreeMap<SessionId, RecordedSessionBinding>,
    session: SessionId,
    composed: ComposedWorkspaceIdentity,
) -> bool {
    bindings.iter().any(|(bound, binding)| {
        *bound != session
            && binding
                .derived_identity()
                .is_some_and(|bound_identity| bound_identity.shares_a_directory_with(composed))
    })
}

/// Whether the directory a derived root is reached through is itself one the
/// configured composition holds.
///
/// `<name>.sessions` bind-mounted onto the configured root is a real directory,
/// so the parent classification admits it and every identifier child beneath it
/// is a directory *inside* the configured workspace — readable, writable, and
/// executable by every session still bound to that root, which is the
/// containment the sibling derivation exists to establish. The bound pair alone
/// cannot show it: the child is nested rather than equal, and ancestry is not
/// equality. This is distinct from the admitted residual where only the
/// parent's *contents* are a bind mount, which stands as its own directory and
/// shares no identity with the configured pair.
///
/// Both the pinned and the standing configured pairs are compared, for the same
/// reason [`shares_a_directory_with_the_configured_root`] compares both.
const fn parent_aliases_the_configured_root(
    parent: ComposedRootIdentity,
    pinned: ComposedWorkspaceIdentity,
    standing: ComposedWorkspaceIdentity,
) -> bool {
    parent.is_the_same_directory_as(pinned.root)
        || parent.is_the_same_directory_as(pinned.administration)
        || parent.is_the_same_directory_as(standing.root)
        || parent.is_the_same_directory_as(standing.administration)
}

/// Whether a composed workspace is the very directory its pathname was reached
/// through.
///
/// A session's identifier directory that is a bind mount of `<name>.sessions`
/// itself composes to a root whose identity is the parent's own. The parent is
/// the directory holding every sibling session's root, so admitting it would
/// give one session a workspace that contains every other session's, and its
/// mutation and execution tools reach all of them.
///
/// No other comparison shows it. The configured checks compare against the
/// configured composition, which this parent is not, and `another_session_bound`
/// compares against the pairs other sessions bound, which are distinct
/// directories nested inside this one — ancestry is not equality. The parent is
/// carried precisely because nothing downstream can recover it, so this is the
/// one site holding both values.
///
/// Both composed directories are compared: a `.git` standing on the parent
/// nests the siblings inside this session's administration directory just as a
/// root standing on it nests them inside its worktree.
const fn composition_aliases_its_own_parent(
    composed: ComposedWorkspaceIdentity,
    parent: ComposedRootIdentity,
) -> bool {
    composed.root.is_the_same_directory_as(parent)
        || composed.administration.is_the_same_directory_as(parent)
}

/// Whether any session other than `session` holds a derived binding at all.
///
/// Asked before the configured pathname is captured, so a deployment where no
/// session was ever provisioned a root of its own pays no syscall for a
/// comparison that has nothing to compare against.
fn a_derived_binding_exists(
    bindings: &BTreeMap<SessionId, RecordedSessionBinding>,
    session: SessionId,
) -> bool {
    bindings
        .iter()
        .any(|(bound, binding)| *bound != session && binding.derived_identity().is_some())
}

/// Whether any other session's derived binding shares a directory with the
/// configured composition as its pathname stands now.
///
/// The mirror of the comparison a derived dispatch makes against the configured
/// root. A configured-root request has to make it too: the configured
/// composition is never re-resolved, so a `.git` bind-mounted from a derived
/// session's workspace over the configured root's own leaves the configured
/// executors reaching that workspace while the session that bound it keeps a
/// separate serialization domain. Checking only on the derived branch would
/// protect only the sessions that take it.
fn a_derived_binding_shares_the_configured_root(
    bindings: &BTreeMap<SessionId, RecordedSessionBinding>,
    session: SessionId,
    pinned: ComposedWorkspaceIdentity,
    standing: ComposedWorkspaceIdentity,
) -> bool {
    bindings.iter().any(|(bound, binding)| {
        *bound != session
            && binding.derived_identity().is_some_and(|identity| {
                shares_a_directory_with_the_configured_root(identity, pinned, standing)
            })
    })
}

/// Filesystem identity of one directory.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ComposedRootIdentity {
    /// Device the directory lives on.
    pub device: u64,
    /// Inode number within that device.
    pub inode: u64,
}

impl ComposedRootIdentity {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }

    /// Adopts one directory identity a composed suite pinned.
    const fn from_pinned(identity: WorkspaceRootIdentity) -> Self {
        Self {
            device: identity.device,
            inode: identity.inode,
        }
    }

    /// Whether two identities name one directory.
    const fn is_the_same_directory_as(self, other: Self) -> bool {
        self.device == other.device && self.inode == other.inode
    }
}

/// Captures the identity the root pathname resolves to right now.
fn composed_root_identity(
    root: &Path,
) -> Result<ComposedRootIdentity, DaemonToolsConstructionError> {
    let metadata = fs::symlink_metadata(root)
        .map_err(|_| DaemonToolsConstructionError::WorkspaceRootUnstable)?;
    if !metadata.is_dir() {
        return Err(DaemonToolsConstructionError::WorkspaceRootUnstable);
    }
    Ok(ComposedRootIdentity::from_metadata(&metadata))
}

/// The two directories one composed workspace binds.
///
/// Two roots can be distinct directories and still share one repository — two
/// bind mounts over one checkout, say — so isolation is a property of both the
/// worktree and the administration directory, not of the worktree alone.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ComposedWorkspaceIdentity {
    /// Identity of the worktree root itself.
    pub root: ComposedRootIdentity,
    /// Identity of the `.git` directory immediately inside that root.
    pub administration: ComposedRootIdentity,
}

impl ComposedWorkspaceIdentity {
    /// Captures both identities one composed workspace root binds.
    ///
    /// The administration directory is `.git` immediately inside the root,
    /// which the Git family has already required by the time this is captured.
    fn capture(root: &Path) -> Result<Self, DaemonToolsConstructionError> {
        Ok(Self {
            root: composed_root_identity(root)?,
            administration: composed_root_identity(&root.join(GIT_ADMINISTRATION_DIRECTORY))?,
        })
    }

    /// Adopts the two directories a composed Git suite pinned.
    ///
    /// The Git suite accepted both identities on either side of its repository
    /// open, so its pair is what this composition holds. Resolving the pathname
    /// again once the suite is built would instead record whatever stands there
    /// then: a `.git` replaced in between would be recorded while the Git
    /// executor stays bound to the repository it opened, so a later collision
    /// check would protect the replacement rather than the retained authority.
    const fn from_pinned(directories: PinnedRepositoryDirectories) -> Self {
        Self {
            root: ComposedRootIdentity::from_pinned(directories.root),
            administration: ComposedRootIdentity::from_pinned(directories.administration),
        }
    }

    /// Whether these two composed workspaces share any directory, in any role.
    ///
    /// Every pairing is compared rather than only root-to-root and
    /// administration-to-administration, because one composition's worktree
    /// root can be the directory another composition administers — a nested
    /// repository exposed by a bind mount, say. Comparing within roles alone
    /// admits both, and the first composition's mutation and execution tools
    /// then write the second composition's repository administration state.
    const fn shares_a_directory_with(self, other: Self) -> bool {
        self.root.is_the_same_directory_as(other.root)
            || self.root.is_the_same_directory_as(other.administration)
            || self.administration.is_the_same_directory_as(other.root)
            || self
                .administration
                .is_the_same_directory_as(other.administration)
    }
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

struct SharedToolExecutor<Executor> {
    inner: Arc<Mutex<Executor>>,
}

impl<Executor> SharedToolExecutor<Executor> {
    fn new(executor: Executor) -> Self {
        Self {
            inner: Arc::new(Mutex::new(executor)),
        }
    }

    /// Whether this handle is the only one, so releasing it releases the
    /// serialization domain rather than leaving a second one beside it.
    fn is_sole_handle(&self) -> bool {
        Arc::strong_count(&self.inner) == 1
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
    /// The derived path could not be classified, is not a directory, or has
    /// gone away under a session that already bound it.
    UnresolvableRoot,
    /// The derived root is the same directory as the configured root or as
    /// another session's, so binding it would defeat the isolation the
    /// derivation exists to establish.
    SharedRootIdentity,
    /// A different directory now stands at the pathname this session bound.
    ReplacedRootIdentity,
    /// The configured root's own directories could not be captured, so whether
    /// this session's root is one of them could not be decided.
    UnverifiableConfiguredRoot,
}

impl SessionWorkspaceFailure {
    /// Names the failure for startup-free runtime telemetry.
    const fn discriminant(self) -> &'static str {
        match self {
            Self::Composition(_) => "composition_rejected",
            Self::ObjectFormatDisagreement => "object_format_disagreement",
            Self::UnresolvableRoot => "derived_root_unresolvable",
            Self::SharedRootIdentity => "derived_root_shared",
            Self::ReplacedRootIdentity => "derived_root_replaced",
            Self::UnverifiableConfiguredRoot => "configured_root_unverifiable",
        }
    }
}

/// Sanitized details naming why a session's workspace-bound tools are
/// unavailable.
///
/// The reason travels in the tool result rather than in a second operator
/// event: the tool loop already emits one failed-attempt event at its single
/// admission site, and a closed discriminant in the durable result is better
/// provenance than a log line beside it. Each value is a fixed string naming a
/// closed reason, so nothing about the deployment's paths reaches the model.
#[derive(Clone, Debug)]
struct SessionWorkspaceFailureDetails {
    composition: ToolExecutionErrorDetail,
    object_format: ToolExecutionErrorDetail,
    unresolvable_root: ToolExecutionErrorDetail,
    shared_root: ToolExecutionErrorDetail,
    replaced_root: ToolExecutionErrorDetail,
    unverifiable_configured_root: ToolExecutionErrorDetail,
}

impl SessionWorkspaceFailureDetails {
    fn try_new() -> Result<Self, DaemonToolsConstructionError> {
        let detail = |value: &str| {
            ToolExecutionErrorDetail::try_new(value.to_owned())
                .map_err(|_| DaemonToolsConstructionError::SessionWorkspaceDetail)
        };
        Ok(Self {
            composition: detail(SESSION_WORKSPACE_COMPOSITION_DETAIL)?,
            object_format: detail(SESSION_WORKSPACE_OBJECT_FORMAT_DETAIL)?,
            unresolvable_root: detail(SESSION_WORKSPACE_UNRESOLVABLE_DETAIL)?,
            shared_root: detail(SESSION_WORKSPACE_SHARED_DETAIL)?,
            replaced_root: detail(SESSION_WORKSPACE_REPLACED_DETAIL)?,
            unverifiable_configured_root: detail(SESSION_WORKSPACE_UNVERIFIABLE_CONFIGURED_DETAIL)?,
        })
    }

    /// Names the closed reason one failure carries into the tool result.
    fn detail(&self, failure: SessionWorkspaceFailure) -> ToolExecutionErrorDetail {
        match failure {
            SessionWorkspaceFailure::Composition(_) => self.composition.clone(),
            SessionWorkspaceFailure::ObjectFormatDisagreement => self.object_format.clone(),
            SessionWorkspaceFailure::UnresolvableRoot => self.unresolvable_root.clone(),
            SessionWorkspaceFailure::SharedRootIdentity => self.shared_root.clone(),
            SessionWorkspaceFailure::ReplacedRootIdentity => self.replaced_root.clone(),
            SessionWorkspaceFailure::UnverifiableConfiguredRoot => {
                self.unverifiable_configured_root.clone()
            }
        }
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
    cargo_registry_cache: Option<PathBuf>,
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
            git_identity: self.git_identity.clone(),
            exec_runner: self.exec_runner.clone(),
            cargo_registry_cache: self.cargo_registry_cache.clone(),
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
    fn try_new(
        composition: ConfiguredWorkspaceComposition<FileSystem, ExecRunner>,
    ) -> Result<Self, DaemonToolsConstructionError> {
        let ConfiguredWorkspaceComposition {
            families,
            roots,
            git_identity,
            exec_runner,
            cargo_registry_cache,
        } = composition;
        let failure_details = SessionWorkspaceFailureDetails::try_new()?;
        Ok(Self {
            roots,
            git_identity,
            exec_runner,
            cargo_registry_cache,
            configured: families.executors,
            failure_details,
            state: Arc::new(Mutex::new(SessionWorkspaceState::new())),
        })
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
    async fn execute(
        &mut self,
        invocation: ToolExecutionInvocation,
    ) -> Result<CorrelatedToolExecutorEvidence, DaemonToolExecutorError> {
        let session = invocation.correlation().session();
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

#[cfg(test)]
mod tests;
