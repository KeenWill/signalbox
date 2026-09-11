use super::{
    DaemonToolsConstructionError, composed_identity::ComposedWorkspaceIdentity,
    pinned_file_system::PinFurtherWorkspaceRoot, workspace_executors::SessionWorkspaceExecutors,
};
use signalbox_domain::SessionId;
use signalbox_tools_exec::ProcessRunner;
use signalbox_tools_workspace::{
    WorkspaceFileSystem, WorkspaceMutationFileSystem, WorkspaceRootIdentity,
};
use std::{
    collections::BTreeMap,
    fmt, fs,
    future::Future,
    io,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
};

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
pub(super) const MAX_RETAINED_SESSION_WORKSPACES: usize = 8;

/// Direct administration entry used by repository fixtures.
#[cfg(test)]
pub(super) const GIT_ADMINISTRATION_DIRECTORY: &str = ".git";

pub(super) const SESSION_WORKSPACE_BINDING_EVIDENCE_DETAIL: &str =
    "session workspace binding evidence could not be recorded";

pub(super) const SESSION_WORKSPACE_COMPOSITION_DETAIL: &str =
    "session workspace could not be composed";

pub(super) const SESSION_WORKSPACE_OBJECT_FORMAT_DETAIL: &str =
    "session workspace repository uses another object format";

pub(super) const SESSION_WORKSPACE_UNRESOLVABLE_DETAIL: &str =
    "session workspace root is unresolvable";

pub(super) const SESSION_WORKSPACE_SHARED_DETAIL: &str =
    "session workspace root is shared with another session";

pub(super) const SESSION_WORKSPACE_REPLACED_DETAIL: &str =
    "session workspace root changed since this session bound it";

pub(super) const SESSION_WORKSPACE_UNVERIFIABLE_CONFIGURED_DETAIL: &str =
    "configured workspace root could not be revalidated";

/// Derives each session's workspace root from the configured root by a fixed
/// formula.
///
/// A session names no path: the derivation takes only the configured root and
/// the session's own identity, so the set of roots the daemon can ever open is
/// determined by deployment configuration alone.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionWorkspaceRoots {
    pub(super) configured: PathBuf,
    pub(super) derived_parent: PathBuf,
}

pub(super) type WorkspaceInstructionRootFuture<'a> = Pin<
    Box<dyn Future<Output = Result<PathBuf, WorkspaceInstructionRootResolutionError>> + Send + 'a>,
>;

pub(super) trait WorkspaceInstructionRootAuthority: Send + Sync {
    fn resolve(&self, session: SessionId) -> WorkspaceInstructionRootFuture<'_>;
}

/// Cloneable access to the workspace-binding authority used by daemon tools.
///
/// Instruction discovery uses this handle so it cannot independently choose a
/// different configured-versus-derived root for a session whose binding is
/// already sticky.
#[derive(Clone)]
pub struct WorkspaceInstructionRootResolver {
    pub(super) authority: Arc<dyn WorkspaceInstructionRootAuthority>,
}

impl fmt::Debug for WorkspaceInstructionRootResolver {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkspaceInstructionRootResolver")
            .finish_non_exhaustive()
    }
}

impl WorkspaceInstructionRootResolver {
    pub(super) fn new<FileSystem, ExecRunner>(
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
    pub(super) fn standing_parent(&self) -> Option<ComposedRootIdentity> {
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
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum RecordedSessionBinding {
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
    pub(super) fn derived_identity(&self) -> Option<ComposedWorkspaceIdentity> {
        match self {
            Self::ConfiguredRoot => None,
            Self::DerivedRoot { identity, .. } => Some(identity.clone()),
        }
    }

    /// Returns the parent this binding walked through, if it pinned a derived
    /// root.
    pub(super) const fn derived_parent(&self) -> Option<ComposedRootIdentity> {
        match self {
            Self::ConfiguredRoot => None,
            Self::DerivedRoot { parent, .. } => Some(*parent),
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
pub(super) fn probe_is_stale(
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
pub(super) enum SessionRootDecision {
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
pub(super) fn decide_session_root(
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
pub(super) fn shares_a_directory_with_the_configured_root(
    composed: ComposedWorkspaceIdentity,
    pinned: ComposedWorkspaceIdentity,
    standing: ComposedWorkspaceIdentity,
) -> bool {
    composed.shares_a_directory_with(&pinned) || composed.shares_a_directory_with(&standing)
}

/// Whether a session other than `session` already bound the directory a
/// composition just found.
///
/// Asked of the directory rather than of the pathname, because two pathnames
/// can name one directory and each would compose successfully on its own.
pub(super) fn another_session_bound(
    bindings: &BTreeMap<SessionId, RecordedSessionBinding>,
    session: SessionId,
    composed: ComposedWorkspaceIdentity,
    still_reachable: impl Fn(SessionId) -> bool,
) -> bool {
    bindings.iter().any(|(bound, binding)| {
        let Some(bound_identity) = binding.derived_identity() else {
            return false;
        };
        if *bound == session
            || !bound_identity.shares_a_directory_with(&composed)
            || !still_reachable(*bound)
        {
            return false;
        }
        tracing::warn!(
            session_id = %session.into_uuid(),
            bound_session_id = %bound.into_uuid(),
            proposed = ?composed,
            bound_identity = ?bound_identity,
            "workspace directory shares a recorded session binding"
        );
        true
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
pub(super) fn parent_aliases_the_configured_root(
    parent: ComposedRootIdentity,
    pinned: ComposedWorkspaceIdentity,
    standing: ComposedWorkspaceIdentity,
) -> bool {
    pinned.contains_directory(parent) || standing.contains_directory(parent)
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
pub(super) fn composition_aliases_its_own_parent(
    composed: ComposedWorkspaceIdentity,
    parent: ComposedRootIdentity,
) -> bool {
    composed.contains_directory(parent)
}

/// Whether any session other than `session` holds a derived binding at all.
///
/// Asked before the configured pathname is captured, so a deployment where no
/// session was ever provisioned a root of its own pays no syscall for a
/// comparison that has nothing to compare against.
pub(super) fn a_derived_binding_exists(
    bindings: &BTreeMap<SessionId, RecordedSessionBinding>,
    session: SessionId,
) -> bool {
    bindings
        .iter()
        .any(|(bound, binding)| *bound != session && binding.derived_identity().is_some())
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
    pub(super) const fn from_pinned(identity: WorkspaceRootIdentity) -> Self {
        Self {
            device: identity.device,
            inode: identity.inode,
        }
    }

    /// Whether two identities name one directory.
    pub(super) const fn is_the_same_directory_as(self, other: Self) -> bool {
        self.device == other.device && self.inode == other.inode
    }
}

/// Captures the identity the root pathname resolves to right now.
pub(super) fn composed_root_identity(
    root: &Path,
) -> Result<ComposedRootIdentity, DaemonToolsConstructionError> {
    let metadata = fs::symlink_metadata(root)
        .map_err(|_| DaemonToolsConstructionError::WorkspaceRootUnstable)?;
    if !metadata.is_dir() {
        return Err(DaemonToolsConstructionError::WorkspaceRootUnstable);
    }
    Ok(ComposedRootIdentity::from_metadata(&metadata))
}
