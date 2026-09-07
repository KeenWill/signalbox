use super::{
    DaemonToolsConstructionError,
    session_workspace_roots::{
        ComposedRootIdentity, GIT_ADMINISTRATION_DIRECTORY, composed_root_identity,
    },
};
use signalbox_tools_git::PinnedRepositoryDirectories;
use std::path::Path;

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
    pub(super) fn capture(root: &Path) -> Result<Self, DaemonToolsConstructionError> {
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
    pub(super) const fn from_pinned(directories: PinnedRepositoryDirectories) -> Self {
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
    pub(super) const fn shares_a_directory_with(self, other: Self) -> bool {
        self.root.is_the_same_directory_as(other.root)
            || self.root.is_the_same_directory_as(other.administration)
            || self.administration.is_the_same_directory_as(other.root)
            || self
                .administration
                .is_the_same_directory_as(other.administration)
    }
}
