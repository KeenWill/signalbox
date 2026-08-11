//! Process-lifetime compiled daemon tool catalog and executor dispatch.
//!
//! The catalog is one process-lifetime immutable compiled value; the executors
//! a workspace root binds are per session, resolved through
//! [`SessionWorkspaceRoots`]. See `docs/spec/tool-loop.md` and
//! `docs/spec/git-authority-threat-model.md`.

use std::{
    collections::BTreeMap,
    error::Error,
    fmt, fs, io,
    os::unix::fs::MetadataExt,
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
        let local_git = LocalGitTools::try_new(filesystem, root, git_identity)
            .map_err(|_| DaemonToolsConstructionError::LocalGit)?;
        let git_object_format = local_git.object_format();
        let pinned_directories = local_git.pinned_directories();
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
            roots: SessionWorkspaceRoots::try_new(workspace_root)?,
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
            roots: SessionWorkspaceRoots::try_new(workspace_root)?,
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
        } = composition;
        let failure_details = SessionWorkspaceFailureDetails::try_new()?;
        Ok(Self {
            roots,
            git_identity,
            exec_runner,
            configured: families.executors,
            failure_details,
            state: Arc::new(Mutex::new(SessionWorkspaceState::new())),
        })
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
    use std::{ffi::OsStr, fmt, fs, time::SystemTime};

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

    /// A workspace identity a recorded binding pinned. Every member only needs
    /// to be some value a real `stat` could report.
    const FIXTURE_BOUND_IDENTITY: ComposedWorkspaceIdentity = ComposedWorkspaceIdentity {
        root: ComposedRootIdentity {
            device: 0x10,
            inode: 0x20,
        },
        administration: ComposedRootIdentity {
            device: 0x10,
            inode: 0x21,
        },
    };

    /// A workspace sharing neither directory with [`FIXTURE_BOUND_IDENTITY`].
    const FIXTURE_OTHER_IDENTITY: ComposedWorkspaceIdentity = ComposedWorkspaceIdentity {
        root: ComposedRootIdentity {
            device: 0x10,
            inode: 0x30,
        },
        administration: ComposedRootIdentity {
            device: 0x10,
            inode: 0x31,
        },
    };

    /// A distinct worktree over the directory [`FIXTURE_BOUND_IDENTITY`]
    /// administers, which is what two bind mounts over one repository produce.
    const FIXTURE_SHARED_ADMINISTRATION_IDENTITY: ComposedWorkspaceIdentity =
        ComposedWorkspaceIdentity {
            root: ComposedRootIdentity {
                device: 0x10,
                inode: 0x40,
            },
            administration: FIXTURE_BOUND_IDENTITY.administration,
        };

    /// A workspace whose worktree root is the directory
    /// [`FIXTURE_BOUND_IDENTITY`] administers, which is what a nested
    /// repository reached through a bind mount produces.
    const FIXTURE_WORKTREE_OVER_BOUND_ADMINISTRATION_IDENTITY: ComposedWorkspaceIdentity =
        ComposedWorkspaceIdentity {
            root: FIXTURE_BOUND_IDENTITY.administration,
            administration: ComposedRootIdentity {
                device: 0x10,
                inode: 0x50,
            },
        };

    /// A workspace administering the directory [`FIXTURE_BOUND_IDENTITY`] uses
    /// as its worktree root, the other way a nested repository collides.
    const FIXTURE_ADMINISTRATION_OVER_BOUND_WORKTREE_IDENTITY: ComposedWorkspaceIdentity =
        ComposedWorkspaceIdentity {
            root: ComposedRootIdentity {
                device: 0x10,
                inode: 0x60,
            },
            administration: FIXTURE_BOUND_IDENTITY.root,
        };

    /// The pair the configured pathname names after its `.git` was renamed and
    /// recreated, which leaves its worktree root alone.
    const FIXTURE_CONFIGURED_STANDING_IDENTITY: ComposedWorkspaceIdentity =
        ComposedWorkspaceIdentity {
            root: FIXTURE_BOUND_IDENTITY.root,
            administration: ComposedRootIdentity {
                device: 0x10,
                inode: 0x70,
            },
        };

    /// A derived workspace exposing the `.git` directory the configured
    /// pathname names now, sharing nothing with the pair it pinned at startup.
    const FIXTURE_SHARES_CONFIGURED_STANDING_IDENTITY: ComposedWorkspaceIdentity =
        ComposedWorkspaceIdentity {
            root: ComposedRootIdentity {
                device: 0x10,
                inode: 0x80,
            },
            administration: FIXTURE_CONFIGURED_STANDING_IDENTITY.administration,
        };

    /// The directory a derived root's pathname is reached through. Distinct
    /// from every bound directory, since a parent is walked rather than bound.
    const FIXTURE_PARENT_IDENTITY: ComposedRootIdentity = ComposedRootIdentity {
        device: 0x10,
        inode: 0x90,
    };

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

    /// The derivation for one fixture configured root.
    ///
    /// Every fixture root is an absolute path with a parent and a final
    /// component, so the derivation is always constructible for one.
    fn derivation(configured: &Path) -> SessionWorkspaceRoots {
        SessionWorkspaceRoots::try_new(configured)
            .expect("a fixture configured root has a parent and a final component")
    }

    /// Creates a direct main worktree exactly where the derivation places one
    /// session's root, so the test never restates the formula.
    fn provisioned_session_workspace(configured: &Path, session: SessionId, marker: &str) {
        let derived = derivation(configured).derived_path(session);
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
    fn known_failure_detail(evidence: ToolExecutorEvidence) -> String {
        match evidence {
            ToolExecutorEvidence::KnownFailed { detail } => detail
                .expect("a session workspace failure carries sanitized detail")
                .as_str()
                .to_owned(),
            ToolExecutorEvidence::CompletedText(text) => {
                panic!("the workspace tool completed: {text}")
            }
            ToolExecutorEvidence::Ambiguous => panic!("the workspace tool was ambiguous"),
        }
    }

    /// Replaces a bound workspace's `.git` directory, leaving its worktree root,
    /// its pathname, and every file a read returns exactly where they were.
    fn replace_administration_directory(root: &Path) {
        let displaced = root
            .parent()
            .expect("a derived session root has a parent")
            .join("displaced.git");
        fs::rename(root.join(".git"), displaced)
            .expect("the bound administration directory moves aside");
        git2::Repository::init(root).expect("a replacement repository initializes");
    }

    /// Replaces the directory a session's root is reached through, leaving that
    /// root and its `.git` the same two directories at the same pathname.
    ///
    /// The session's own directory is moved rather than recreated, so it keeps
    /// its identity and the bound pair still compares equal; only the component
    /// walked through to reach it is a different directory afterwards.
    fn replace_derived_parent(parent: &Path, session_directory: &OsStr) {
        let displaced = parent.with_extension("displaced");
        fs::rename(parent, &displaced).expect("the classified parent moves aside");
        fs::create_dir(parent).expect("a replacement parent stands at the same pathname");
        fs::rename(
            displaced.join(session_directory),
            parent.join(session_directory),
        )
        .expect("the session's own directory moves under the replacement");
        fs::remove_dir(&displaced).expect("the displaced parent is empty and is removed");
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

        let derived = derivation(configured).derived_path(first);

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

        let resolved = derivation(&configured).resolve(session(FIRST_SESSION_IDENTITY));

        assert_eq!(resolved, SessionWorkspaceRoot::ConfiguredRoot);
    }

    /// A session whose derived directory exists binds that directory.
    #[test]
    fn a_provisioned_session_binds_its_derived_root() {
        let parent = tempfile::tempdir().expect("fixture parent exists");
        let configured = configured_workspace(parent.path());
        let first = session(FIRST_SESSION_IDENTITY);
        provisioned_session_workspace(&configured, first, FIRST_SESSION_MARKER);
        let roots = derivation(&configured);
        let expected = roots.derived_path(first);

        let resolved = roots.resolve(first);

        let SessionWorkspaceRoot::Derived { path, .. } = resolved else {
            panic!("a provisioned session resolves to its derived root");
        };
        assert_eq!(path, expected);
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

        // Joined rather than awaited one after the other: the claim is about
        // two sessions resolving and composing against one shared state, and
        // awaiting the first to completion before the second is even started
        // would leave that state uncontended throughout.
        let (first_read, second_read) = tokio::join!(
            daemon_evidence(
                catalog.clone(),
                executor.clone(),
                first,
                read_marker_proposal(),
            ),
            daemon_evidence(catalog, executor, second, read_marker_proposal()),
        );

        assert_eq!(read_content(first_read), FIRST_SESSION_MARKER);
        assert_eq!(read_content(second_read), SECOND_SESSION_MARKER);
    }

    /// A session keeps reaching the derived root it bound only while both the
    /// worktree and the `.git` directory inside it still stand. Provisioning
    /// that replaces the administration directory alone leaves the worktree,
    /// the pathname, and every file a read returns in place, and the session's
    /// next request fails closed rather than being served from a retained
    /// composition whose Git executor is pinned to the displaced repository.
    #[tokio::test]
    async fn a_replaced_administration_directory_fails_the_next_request() {
        let parent = tempfile::tempdir().expect("fixture parent exists");
        let configured = configured_workspace(parent.path());
        let first = session(FIRST_SESSION_IDENTITY);
        provisioned_session_workspace(&configured, first, FIRST_SESSION_MARKER);
        let derived = derivation(&configured).derived_path(first);
        let (catalog, executor) = offline_daemon_composition(&configured);
        let bound = daemon_evidence(
            catalog.clone(),
            executor.clone(),
            first,
            read_marker_proposal(),
        )
        .await;
        replace_administration_directory(&derived);

        let after_replacement =
            daemon_evidence(catalog, executor, first, read_marker_proposal()).await;

        assert_eq!(read_content(bound), FIRST_SESSION_MARKER);
        assert_eq!(
            known_failure_detail(after_replacement),
            SESSION_WORKSPACE_REPLACED_DETAIL
        );
    }

    /// The directories a session bound can stand unchanged while the directory
    /// walked through to reach them does not. A parent renamed away and
    /// replaced, with this session's own directory moved under the replacement,
    /// leaves the worktree, its `.git`, the pathname, and every file a read
    /// returns exactly as they were — only the component in between is another
    /// directory. The session's next request fails closed rather than reaching
    /// its tree through a directory nothing classified.
    #[tokio::test]
    async fn a_replaced_derived_parent_fails_the_next_request() {
        let parent = tempfile::tempdir().expect("fixture parent exists");
        let configured = configured_workspace(parent.path());
        let first = session(FIRST_SESSION_IDENTITY);
        provisioned_session_workspace(&configured, first, FIRST_SESSION_MARKER);
        let derived = derivation(&configured).derived_path(first);
        let derived_parent = derived
            .parent()
            .expect("a derived session root has a parent")
            .to_owned();
        let session_directory = derived
            .file_name()
            .expect("the derived path names a session directory")
            .to_owned();
        let (catalog, executor) = offline_daemon_composition(&configured);
        let bound = daemon_evidence(
            catalog.clone(),
            executor.clone(),
            first,
            read_marker_proposal(),
        )
        .await;
        replace_derived_parent(&derived_parent, &session_directory);

        let after_replacement =
            daemon_evidence(catalog, executor, first, read_marker_proposal()).await;

        assert_eq!(read_content(bound), FIRST_SESSION_MARKER);
        assert_eq!(
            fs::read_to_string(derived.join(SESSION_MARKER_PATH))
                .expect("the session's own file is where it was"),
            FIRST_SESSION_MARKER
        );
        assert_eq!(
            known_failure_detail(after_replacement),
            SESSION_WORKSPACE_REPLACED_DETAIL
        );
    }

    /// Whether a derived root is one of the configured root's own directories
    /// cannot be decided once the configured pathname stops resolving. The
    /// configured adapter still holds its root descriptor and still reaches
    /// whatever stands under it, so a session already bound to a derived root
    /// fails closed on its next request rather than dispatching on a
    /// comparison against the startup pair alone.
    #[tokio::test]
    async fn an_uncapturable_configured_root_fails_a_bound_session() {
        let parent = tempfile::tempdir().expect("fixture parent exists");
        let configured = configured_workspace(parent.path());
        let first = session(FIRST_SESSION_IDENTITY);
        provisioned_session_workspace(&configured, first, FIRST_SESSION_MARKER);
        let (catalog, executor) = offline_daemon_composition(&configured);
        let bound = daemon_evidence(
            catalog.clone(),
            executor.clone(),
            first,
            read_marker_proposal(),
        )
        .await;
        fs::remove_dir_all(configured.join(GIT_ADMINISTRATION_DIRECTORY))
            .expect("the configured administration directory is removed");

        let after_removal = daemon_evidence(catalog, executor, first, read_marker_proposal()).await;

        assert_eq!(read_content(bound), FIRST_SESSION_MARKER);
        assert_eq!(
            known_failure_detail(after_removal),
            SESSION_WORKSPACE_UNVERIFIABLE_CONFIGURED_DETAIL
        );
    }

    /// The same refusal on the admission path, which reaches the comparison
    /// through its own capture: a session composing its derived root for the
    /// first time is not admitted while the configured pair is unreadable.
    #[tokio::test]
    async fn an_uncapturable_configured_root_fails_an_unbound_session() {
        let parent = tempfile::tempdir().expect("fixture parent exists");
        let configured = configured_workspace(parent.path());
        let first = session(FIRST_SESSION_IDENTITY);
        provisioned_session_workspace(&configured, first, FIRST_SESSION_MARKER);
        let (catalog, executor) = offline_daemon_composition(&configured);
        fs::remove_dir_all(configured.join(GIT_ADMINISTRATION_DIRECTORY))
            .expect("the configured administration directory is removed");

        let first_request = daemon_evidence(catalog, executor, first, read_marker_proposal()).await;

        assert_eq!(
            known_failure_detail(first_request),
            SESSION_WORKSPACE_UNVERIFIABLE_CONFIGURED_DETAIL
        );
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

    /// Whether a retained fixture stands in for a value a request still holds.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum FixtureRequestState {
        Idle,
        InFlight,
    }

    /// Stands in for a composed executor set so the retained set's bound and
    /// eviction order can be exercised without opening a repository.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct RetainedFixture {
        marker: u32,
        request_state: FixtureRequestState,
    }

    impl RetainedFixture {
        const fn idle(marker: u32) -> Self {
            Self {
                marker,
                request_state: FixtureRequestState::Idle,
            }
        }

        const fn in_flight(marker: u32) -> Self {
            Self {
                marker,
                request_state: FixtureRequestState::InFlight,
            }
        }
    }

    impl RetainedInFlight for RetainedFixture {
        fn is_in_flight(&self) -> bool {
            match self.request_state {
                FixtureRequestState::Idle => false,
                FixtureRequestState::InFlight => true,
            }
        }
    }

    /// Retains filler sessions until the bound is reached.
    ///
    /// The iteration lives here rather than in a test body, which stays
    /// straight-line: what the test is about is which entry the next retention
    /// evicts, not how the set was filled.
    fn fill_remaining_capacity(
        retained: &mut RetainedSessionWorkspaces<RetainedFixture>,
        filler: RetainedFixture,
    ) {
        for offset in 0..MAX_RETAINED_SESSION_WORKSPACES - 2 {
            let identity = FILLER_SESSION_IDENTITY_BASE + offset as u128;
            retained.retain(session(identity), filler);
        }
    }

    /// A path that exists but is not a directory is a misprovisioned session,
    /// not an unprovisioned one, so it never reads as the configured root.
    #[test]
    fn a_nondirectory_at_the_derived_path_is_unresolvable() {
        let parent = tempfile::tempdir().expect("fixture parent exists");
        let configured = configured_workspace(parent.path());
        let first = session(FIRST_SESSION_IDENTITY);
        let roots = derivation(&configured);
        let derived = roots.derived_path(first);
        fs::create_dir_all(
            derived
                .parent()
                .expect("the derived path has a parent directory"),
        )
        .expect("the derived parent exists");
        fs::write(&derived, FIRST_SESSION_MARKER).expect("a file occupies the derived path");

        let resolved = roots.resolve(first);

        assert_eq!(resolved, SessionWorkspaceRoot::Unresolvable);
    }

    /// A symlink at the derived parent would nest every session's root inside
    /// the configured root, where every session still bound to that root can
    /// read, write, and execute it. The parent is classified without following
    /// it, so such a session is misprovisioned rather than derived.
    #[test]
    fn a_symlinked_derived_parent_is_unresolvable() {
        let parent = tempfile::tempdir().expect("fixture parent exists");
        let configured = configured_workspace(parent.path());
        let first = session(FIRST_SESSION_IDENTITY);
        let roots = derivation(&configured);
        let nested = configured.join("sessions");
        let derived = roots.derived_path(first);
        fs::create_dir(&nested).expect("a directory inside the configured root exists");
        fs::create_dir(
            nested.join(
                derived
                    .file_name()
                    .expect("the derived path names a session directory"),
            ),
        )
        .expect("the nested session directory exists");
        std::os::unix::fs::symlink(
            &nested,
            derived
                .parent()
                .expect("the derived path has a parent directory"),
        )
        .expect("the derived parent is a symlink into the configured root");

        let resolved = roots.resolve(first);

        assert_eq!(resolved, SessionWorkspaceRoot::Unresolvable);
    }

    /// A session that has bound nothing yet and has no derived directory binds
    /// the configured root.
    #[test]
    fn an_unbound_session_without_a_directory_decides_the_configured_root() {
        let decision = decide_session_root(None, &SessionWorkspaceRoot::ConfiguredRoot);

        assert_eq!(decision, SessionRootDecision::ConfiguredRoot);
    }

    /// A session that has bound nothing yet and has a derived directory
    /// composes against it.
    #[test]
    fn an_unbound_session_with_a_directory_decides_to_compose() {
        let decision = decide_session_root(
            None,
            &SessionWorkspaceRoot::Derived {
                path: PathBuf::from("/srv/signalbox/workspace.sessions/fixture"),
                parent: FIXTURE_PARENT_IDENTITY,
            },
        );

        assert_eq!(decision, SessionRootDecision::ComposeDerived);
    }

    /// A session that has bound nothing yet and whose derived path cannot be
    /// classified fails closed rather than binding the shared configured root.
    #[test]
    fn an_unbound_session_with_an_unresolvable_path_decides_to_fail() {
        let decision = decide_session_root(None, &SessionWorkspaceRoot::Unresolvable);

        assert_eq!(decision, SessionRootDecision::Unresolvable);
    }

    /// A session already bound to the configured root stays there even once a
    /// directory appears at its derived path, so its tree cannot change under
    /// it mid-session.
    #[test]
    fn a_configured_binding_survives_a_directory_appearing() {
        let decision = decide_session_root(
            Some(RecordedSessionBinding::ConfiguredRoot),
            &SessionWorkspaceRoot::Derived {
                path: PathBuf::from("/srv/signalbox/workspace.sessions/fixture"),
                parent: FIXTURE_PARENT_IDENTITY,
            },
        );

        assert_eq!(decision, SessionRootDecision::ConfiguredRoot);
    }

    /// A session already bound to the configured root stays there when a file,
    /// a symlink, or an unclassifiable entry appears at its derived path too.
    /// It never opens that pathname, so nothing arriving there is reachable by
    /// it, and failing it closed would deny it for the process's lifetime over
    /// a condition it cannot act on — and only for the botched spelling of a
    /// provisioning that arrived too late, while a correct one is ignored.
    #[test]
    fn a_configured_binding_survives_a_misprovisioned_entry_appearing() {
        let decision = decide_session_root(
            Some(RecordedSessionBinding::ConfiguredRoot),
            &SessionWorkspaceRoot::Unresolvable,
        );

        assert_eq!(decision, SessionRootDecision::ConfiguredRoot);
    }

    /// A session already bound to a derived root is never returned to the
    /// configured root by that directory's removal, including after its
    /// executors were evicted from the bounded retained set.
    #[test]
    fn a_derived_binding_fails_closed_once_its_directory_is_gone() {
        let decision = decide_session_root(
            Some(RecordedSessionBinding::DerivedRoot {
                identity: FIXTURE_BOUND_IDENTITY,
                parent: FIXTURE_PARENT_IDENTITY,
            }),
            &SessionWorkspaceRoot::ConfiguredRoot,
        );

        assert_eq!(decision, SessionRootDecision::Unresolvable);
    }

    /// A probe taken before the state lock can observe an absent directory
    /// while a concurrent first request for the same session binds a derived
    /// root under the lock. That pairing is retaken rather than failed, since
    /// the contract has two concurrent first requests converge on the first
    /// record written.
    #[test]
    fn an_absent_probe_against_a_recorded_derived_binding_is_stale() {
        let stale = probe_is_stale(
            Some(RecordedSessionBinding::DerivedRoot {
                identity: FIXTURE_BOUND_IDENTITY,
                parent: FIXTURE_PARENT_IDENTITY,
            }),
            &SessionWorkspaceRoot::ConfiguredRoot,
        );

        assert!(stale);
    }

    /// A probe that classified the derived path as unresolvable is not a stale
    /// absence: it observed something, so retaking it would answer a question
    /// that was already answered.
    #[test]
    fn an_unresolvable_probe_against_a_recorded_derived_binding_is_not_stale() {
        let stale = probe_is_stale(
            Some(RecordedSessionBinding::DerivedRoot {
                identity: FIXTURE_BOUND_IDENTITY,
                parent: FIXTURE_PARENT_IDENTITY,
            }),
            &SessionWorkspaceRoot::Unresolvable,
        );

        assert!(!stale);
    }

    /// A session with no record has no concurrent winner to converge on, so an
    /// absent probe is the answer rather than a stale observation.
    #[test]
    fn an_absent_probe_without_a_record_is_not_stale() {
        let stale = probe_is_stale(None, &SessionWorkspaceRoot::ConfiguredRoot);

        assert!(!stale);
    }

    /// A recorded derived binding names the parent it walked through as well as
    /// the directories it bound, so a caller can tell that the component the
    /// classification accepted is still the one the pathname leads through.
    #[test]
    fn a_derived_binding_names_the_parent_it_walked_through() {
        let binding = RecordedSessionBinding::DerivedRoot {
            identity: FIXTURE_BOUND_IDENTITY,
            parent: FIXTURE_PARENT_IDENTITY,
        };

        assert_eq!(binding.derived_parent(), Some(FIXTURE_PARENT_IDENTITY));
    }

    /// A configured binding walks through no derived parent.
    #[test]
    fn a_configured_binding_names_no_derived_parent() {
        let binding = RecordedSessionBinding::ConfiguredRoot;

        assert_eq!(binding.derived_parent(), None);
    }

    /// A configured root with no lexical final component — `/srv/workspace/..`,
    /// which is absolute and can name a valid worktree — has no directory name
    /// to append the suffix to. The derivation rejects it rather than answering
    /// "unprovisioned" for every session, which would silently return the
    /// deployment to the one shared root.
    #[test]
    fn a_configured_root_without_a_final_component_has_no_derivation() {
        let underivable = SessionWorkspaceRoots::try_new(Path::new("/srv/signalbox/workspace/.."));

        assert_eq!(
            underivable,
            Err(DaemonToolsConstructionError::WorkspaceRootUnderivable)
        );
    }

    /// The filesystem root itself has no parent, and is rejected for the same
    /// reason.
    #[test]
    fn the_filesystem_root_has_no_derivation() {
        let underivable = SessionWorkspaceRoots::try_new(Path::new("/"));

        assert_eq!(
            underivable,
            Err(DaemonToolsConstructionError::WorkspaceRootUnderivable)
        );
    }

    /// A recorded derived binding names the directory it bound, so a caller can
    /// tell a resumed workspace from a replacement at the same pathname.
    #[test]
    fn a_derived_binding_names_the_identity_it_pinned() {
        let binding = RecordedSessionBinding::DerivedRoot {
            identity: FIXTURE_BOUND_IDENTITY,
            parent: FIXTURE_PARENT_IDENTITY,
        };

        assert_eq!(binding.derived_identity(), Some(FIXTURE_BOUND_IDENTITY));
    }

    /// A configured binding pins no derived identity, so it never collides with
    /// a derived root another session composed.
    #[test]
    fn a_configured_binding_names_no_derived_identity() {
        let binding = RecordedSessionBinding::ConfiguredRoot;

        assert_eq!(binding.derived_identity(), None);
    }

    /// `<name>.sessions` bind-mounted onto the configured root is a real
    /// directory, so the parent classification admits it while every child
    /// beneath it is nested inside the configured workspace. The bound pair
    /// cannot show that — ancestry is not equality — so the parent is compared
    /// against the configured pair directly.
    #[test]
    fn a_parent_that_is_the_configured_worktree_is_refused() {
        assert!(parent_aliases_the_configured_root(
            FIXTURE_BOUND_IDENTITY.root,
            FIXTURE_BOUND_IDENTITY,
            FIXTURE_BOUND_IDENTITY
        ));
    }

    /// The administration directory collides the same way as the worktree, so
    /// a parent standing on it is refused too.
    #[test]
    fn a_parent_that_is_the_configured_administration_directory_is_refused() {
        assert!(parent_aliases_the_configured_root(
            FIXTURE_BOUND_IDENTITY.administration,
            FIXTURE_BOUND_IDENTITY,
            FIXTURE_BOUND_IDENTITY
        ));
    }

    /// The configured pathname is never re-resolved, so a parent aliasing what
    /// it names *now* is refused even though it shares nothing with the pair
    /// pinned at startup.
    #[test]
    fn a_parent_that_is_the_standing_configured_administration_directory_is_refused() {
        assert!(parent_aliases_the_configured_root(
            FIXTURE_CONFIGURED_STANDING_IDENTITY.administration,
            FIXTURE_BOUND_IDENTITY,
            FIXTURE_CONFIGURED_STANDING_IDENTITY
        ));
    }

    /// The ordinary case: a parent is walked through rather than bound, and a
    /// sibling directory shares no identity with the configured pair. This is
    /// also the admitted residual — a parent whose *contents* are a bind mount
    /// stands as its own directory and is admitted here.
    #[test]
    fn a_parent_beside_the_configured_root_is_admitted() {
        assert!(!parent_aliases_the_configured_root(
            FIXTURE_PARENT_IDENTITY,
            FIXTURE_BOUND_IDENTITY,
            FIXTURE_CONFIGURED_STANDING_IDENTITY
        ));
    }

    /// A configured-root request remakes the collision comparison the derived
    /// branch makes, so a derived session's workspace bind-mounted over what
    /// the configured pathname names now refuses the configured dispatch too
    /// rather than protecting only requests that take the derived branch.
    #[test]
    fn a_configured_request_refuses_a_derived_binding_reaching_the_configured_root() {
        let first = session(FIRST_SESSION_IDENTITY);
        let second = session(SECOND_SESSION_IDENTITY);
        let bindings = BTreeMap::from([(
            first,
            RecordedSessionBinding::DerivedRoot {
                identity: FIXTURE_SHARES_CONFIGURED_STANDING_IDENTITY,
                parent: FIXTURE_PARENT_IDENTITY,
            },
        )]);

        assert!(a_derived_binding_shares_the_configured_root(
            &bindings,
            second,
            FIXTURE_BOUND_IDENTITY,
            FIXTURE_CONFIGURED_STANDING_IDENTITY
        ));
    }

    /// A derived session isolated from the configured root leaves a configured
    /// request alone.
    #[test]
    fn a_configured_request_admits_an_isolated_derived_binding() {
        let first = session(FIRST_SESSION_IDENTITY);
        let second = session(SECOND_SESSION_IDENTITY);
        let bindings = BTreeMap::from([(
            first,
            RecordedSessionBinding::DerivedRoot {
                identity: FIXTURE_OTHER_IDENTITY,
                parent: FIXTURE_PARENT_IDENTITY,
            },
        )]);

        assert!(!a_derived_binding_shares_the_configured_root(
            &bindings,
            second,
            FIXTURE_BOUND_IDENTITY,
            FIXTURE_CONFIGURED_STANDING_IDENTITY
        ));
    }

    /// A deployment that provisioned no session a root of its own has nothing
    /// to compare a configured request against, so it captures no identity for
    /// the comparison.
    #[test]
    fn a_deployment_with_no_derived_binding_captures_nothing() {
        let first = session(FIRST_SESSION_IDENTITY);
        let second = session(SECOND_SESSION_IDENTITY);
        let bindings = BTreeMap::from([(first, RecordedSessionBinding::ConfiguredRoot)]);

        assert!(!a_derived_binding_exists(&bindings, second));
    }

    /// A session's own derived binding is not something its next configured
    /// request compares against; only another session's is.
    #[test]
    fn a_session_own_derived_binding_is_not_another_binding() {
        let first = session(FIRST_SESSION_IDENTITY);
        let bindings = BTreeMap::from([(
            first,
            RecordedSessionBinding::DerivedRoot {
                identity: FIXTURE_BOUND_IDENTITY,
                parent: FIXTURE_PARENT_IDENTITY,
            },
        )]);

        assert!(!a_derived_binding_exists(&bindings, first));
    }

    /// Two pathnames naming one directory are one workspace, so a second
    /// session composing the directory a first session bound is refused.
    #[test]
    fn a_directory_another_session_bound_is_refused() {
        let first = session(FIRST_SESSION_IDENTITY);
        let second = session(SECOND_SESSION_IDENTITY);
        let bindings = BTreeMap::from([(
            first,
            RecordedSessionBinding::DerivedRoot {
                identity: FIXTURE_BOUND_IDENTITY,
                parent: FIXTURE_PARENT_IDENTITY,
            },
        )]);

        assert!(another_session_bound(
            &bindings,
            second,
            FIXTURE_BOUND_IDENTITY
        ));
    }

    /// A session resuming the directory it bound itself is not a collision.
    #[test]
    fn the_directory_a_session_bound_itself_is_not_a_collision() {
        let first = session(FIRST_SESSION_IDENTITY);
        let bindings = BTreeMap::from([(
            first,
            RecordedSessionBinding::DerivedRoot {
                identity: FIXTURE_BOUND_IDENTITY,
                parent: FIXTURE_PARENT_IDENTITY,
            },
        )]);

        assert!(!another_session_bound(
            &bindings,
            first,
            FIXTURE_BOUND_IDENTITY
        ));
    }

    /// Two worktrees over one repository are one workspace, even though their
    /// root directories differ, so the second is refused.
    #[test]
    fn a_repository_another_session_bound_is_refused() {
        let first = session(FIRST_SESSION_IDENTITY);
        let second = session(SECOND_SESSION_IDENTITY);
        let bindings = BTreeMap::from([(
            first,
            RecordedSessionBinding::DerivedRoot {
                identity: FIXTURE_BOUND_IDENTITY,
                parent: FIXTURE_PARENT_IDENTITY,
            },
        )]);

        assert!(another_session_bound(
            &bindings,
            second,
            FIXTURE_SHARED_ADMINISTRATION_IDENTITY
        ));
    }

    /// A worktree root that is the directory another session administers is one
    /// workspace with it, so composing it is refused: this session's mutation
    /// and execution tools would otherwise write that session's repository
    /// administration state.
    #[test]
    fn a_worktree_over_another_session_administration_directory_is_refused() {
        let first = session(FIRST_SESSION_IDENTITY);
        let second = session(SECOND_SESSION_IDENTITY);
        let bindings = BTreeMap::from([(
            first,
            RecordedSessionBinding::DerivedRoot {
                identity: FIXTURE_BOUND_IDENTITY,
                parent: FIXTURE_PARENT_IDENTITY,
            },
        )]);

        assert!(another_session_bound(
            &bindings,
            second,
            FIXTURE_WORKTREE_OVER_BOUND_ADMINISTRATION_IDENTITY
        ));
    }

    /// The same collision in the other role: administering the directory
    /// another session uses as its worktree root is refused too.
    #[test]
    fn an_administration_directory_over_another_session_worktree_is_refused() {
        let first = session(FIRST_SESSION_IDENTITY);
        let second = session(SECOND_SESSION_IDENTITY);
        let bindings = BTreeMap::from([(
            first,
            RecordedSessionBinding::DerivedRoot {
                identity: FIXTURE_BOUND_IDENTITY,
                parent: FIXTURE_PARENT_IDENTITY,
            },
        )]);

        assert!(another_session_bound(
            &bindings,
            second,
            FIXTURE_ADMINISTRATION_OVER_BOUND_WORKTREE_IDENTITY
        ));
    }

    /// A workspace sharing neither directory with a bound one is admitted.
    #[test]
    fn a_workspace_sharing_no_directory_is_admitted() {
        let first = session(FIRST_SESSION_IDENTITY);
        let second = session(SECOND_SESSION_IDENTITY);
        let bindings = BTreeMap::from([(
            first,
            RecordedSessionBinding::DerivedRoot {
                identity: FIXTURE_BOUND_IDENTITY,
                parent: FIXTURE_PARENT_IDENTITY,
            },
        )]);

        assert!(!another_session_bound(
            &bindings,
            second,
            FIXTURE_OTHER_IDENTITY
        ));
    }

    /// A session bound to the configured root pins no derived directory, so it
    /// never makes another session's derived root read as shared.
    #[test]
    fn a_configured_binding_is_not_a_derived_collision() {
        let first = session(FIRST_SESSION_IDENTITY);
        let second = session(SECOND_SESSION_IDENTITY);
        let bindings = BTreeMap::from([(first, RecordedSessionBinding::ConfiguredRoot)]);

        assert!(!another_session_bound(
            &bindings,
            second,
            FIXTURE_BOUND_IDENTITY
        ));
    }

    /// The configured composition pinned its pair at startup and no request
    /// re-resolves it, so a derived root exposing the `.git` directory the
    /// configured pathname names now is refused even though that directory is
    /// not the one the configured composition recorded.
    #[test]
    fn a_derived_root_over_the_standing_configured_administration_directory_is_refused() {
        assert!(shares_a_directory_with_the_configured_root(
            FIXTURE_SHARES_CONFIGURED_STANDING_IDENTITY,
            FIXTURE_BOUND_IDENTITY,
            FIXTURE_CONFIGURED_STANDING_IDENTITY,
        ));
    }

    /// A derived root sharing the pair the configured composition pinned is
    /// refused whatever its pathname names now.
    #[test]
    fn a_derived_root_over_the_pinned_configured_root_is_refused() {
        assert!(shares_a_directory_with_the_configured_root(
            FIXTURE_BOUND_IDENTITY,
            FIXTURE_BOUND_IDENTITY,
            FIXTURE_CONFIGURED_STANDING_IDENTITY,
        ));
    }

    /// A derived root sharing neither the pinned nor the standing configured
    /// pair is admitted.
    #[test]
    fn a_derived_root_sharing_no_configured_directory_is_admitted() {
        assert!(!shares_a_directory_with_the_configured_root(
            FIXTURE_OTHER_IDENTITY,
            FIXTURE_BOUND_IDENTITY,
            FIXTURE_CONFIGURED_STANDING_IDENTITY,
        ));
    }

    /// The retained set is bounded: a further session drops the least recently
    /// used entry rather than growing the descriptor count without limit.
    #[test]
    fn retaining_beyond_the_bound_evicts_the_least_recently_used_session() {
        const FIRST_MARKER: u32 = 1;
        const SECOND_MARKER: u32 = 2;
        const OVERFLOWING_MARKER: u32 = 3;
        let first_retained = RetainedFixture::idle(FIRST_MARKER);
        let second_retained = RetainedFixture::idle(SECOND_MARKER);
        let overflowing_retained = RetainedFixture::idle(OVERFLOWING_MARKER);
        let mut retained = RetainedSessionWorkspaces::new();
        let first = session(FIRST_SESSION_IDENTITY);
        let second = session(SECOND_SESSION_IDENTITY);
        let overflowing = session(FILLER_SESSION_IDENTITY_BASE - 1);
        retained.retain(first, first_retained);
        retained.retain(second, second_retained);
        fill_remaining_capacity(&mut retained, first_retained);
        // Reading `second` back makes it the most recently used entry, so the
        // entry the overflowing session evicts is unambiguously `first`.
        assert_eq!(retained.get(second), Some(second_retained));

        retained.retain(overflowing, overflowing_retained);

        assert_eq!(retained.get(first), None);
        assert_eq!(retained.get(second), Some(second_retained));
        assert_eq!(retained.get(overflowing), Some(overflowing_retained));
    }

    /// A set a request still holds is never released to make room: releasing it
    /// would let the next request for that session compose a second
    /// serialization domain beside the one already mutating its tree.
    #[test]
    fn a_set_a_request_still_holds_is_never_evicted() {
        const IN_FLIGHT_MARKER: u32 = 1;
        const IDLE_MARKER: u32 = 2;
        const OVERFLOWING_MARKER: u32 = 3;
        let in_flight_retained = RetainedFixture::in_flight(IN_FLIGHT_MARKER);
        let idle_retained = RetainedFixture::idle(IDLE_MARKER);
        let overflowing_retained = RetainedFixture::idle(OVERFLOWING_MARKER);
        let mut retained = RetainedSessionWorkspaces::new();
        let in_flight = session(FIRST_SESSION_IDENTITY);
        let idle = session(SECOND_SESSION_IDENTITY);
        let overflowing = session(FILLER_SESSION_IDENTITY_BASE - 1);
        // The in-flight session is retained first, so least-recently-used order
        // alone would evict it and the assertion below would fail.
        retained.retain(in_flight, in_flight_retained);
        retained.retain(idle, idle_retained);
        fill_remaining_capacity(&mut retained, idle_retained);

        retained.retain(overflowing, overflowing_retained);

        assert_eq!(retained.get(in_flight), Some(in_flight_retained));
        assert_eq!(retained.get(idle), None);
        assert_eq!(retained.get(overflowing), Some(overflowing_retained));
    }

    /// A burst of concurrent sessions may push the retained set above the
    /// bound, but the excess drains once those requests return rather than
    /// persisting one entry per later retention.
    #[test]
    fn idle_overflow_drains_back_to_the_bound() {
        const IN_FLIGHT_MARKER: u32 = 1;
        const IDLE_MARKER: u32 = 2;
        let mut retained = RetainedSessionWorkspaces::new();
        retain_in_flight_over_the_bound(
            &mut retained,
            RetainedFixture::in_flight(IN_FLIGHT_MARKER),
        );
        let overflowed = retained.retained.len();

        release_every_retained_request(&mut retained);
        retained.retain(
            session(FILLER_SESSION_IDENTITY_BASE - 1),
            RetainedFixture::idle(IDLE_MARKER),
        );

        assert!(overflowed > MAX_RETAINED_SESSION_WORKSPACES);
        assert_eq!(retained.retained.len(), MAX_RETAINED_SESSION_WORKSPACES);
    }

    /// Retains more in-flight sessions than the bound admits.
    ///
    /// The iteration lives here rather than in a test body, which stays
    /// straight-line: the claim under test is what happens once those requests
    /// return, not how the overflow was produced.
    fn retain_in_flight_over_the_bound(
        retained: &mut RetainedSessionWorkspaces<RetainedFixture>,
        in_flight: RetainedFixture,
    ) {
        for offset in 0..MAX_RETAINED_SESSION_WORKSPACES + 4 {
            let identity = FILLER_SESSION_IDENTITY_BASE + offset as u128;
            retained.retain(session(identity), in_flight);
        }
    }

    /// Marks every retained fixture idle, standing in for the in-flight
    /// requests returning and releasing their handles.
    fn release_every_retained_request(retained: &mut RetainedSessionWorkspaces<RetainedFixture>) {
        for entry in retained.retained.values_mut() {
            entry.executors.request_state = FixtureRequestState::Idle;
        }
    }

    /// A composed set is in flight exactly while a handle outside the retained
    /// set holds it, which is what a cloned dispatch handle is.
    #[test]
    fn a_shared_executor_reports_a_second_handle() {
        let sole = SharedToolExecutor::new(OfflineWriter);
        let shared = sole.clone();

        assert!(!sole.is_sole_handle());
        assert!(!shared.is_sole_handle());
    }

    /// One handle alone is releasable, so an idle session does not pin the
    /// retained set against every later session.
    #[test]
    fn a_shared_executor_reports_one_handle_as_sole() {
        let sole = SharedToolExecutor::new(OfflineWriter);

        assert!(sole.is_sole_handle());
    }
}
