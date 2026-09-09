use super::{
    DaemonToolsConstructionError,
    session_workspace_roots::{ComposedRootIdentity, composed_root_identity},
};
use signalbox_tools_git::{PinnedRepositoryDirectories, open_repository_administration};
use std::{os::unix::fs::MetadataExt, path::Path};

/// Directories one composed workspace binds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ComposedWorkspaceIdentity {
    /// Identity of the workspace root.
    pub root: ComposedRootIdentity,
    /// Identity of the worktree administration directory, when Git is present.
    pub administration: Option<ComposedRootIdentity>,
    /// Identity of shared references and objects, when Git is present.
    pub common_administration: Option<ComposedRootIdentity>,
}

impl ComposedWorkspaceIdentity {
    pub(super) fn capture(root: &Path) -> Result<Self, DaemonToolsConstructionError> {
        let root_identity = composed_root_identity(root)?;
        let (administration, common_administration) = match open_repository_administration(root)
            .map_err(|_| DaemonToolsConstructionError::LocalGit)?
        {
            Some(directories) => (
                Some(directory_identity(&directories.worktree)?),
                Some(directory_identity(&directories.common)?),
            ),
            None => (None, None),
        };
        if composed_root_identity(root)? != root_identity {
            return Err(DaemonToolsConstructionError::WorkspaceRootUnstable);
        }
        Ok(Self {
            root: root_identity,
            administration,
            common_administration,
        })
    }

    pub(super) const fn from_pinned(directories: PinnedRepositoryDirectories) -> Self {
        Self {
            root: ComposedRootIdentity::from_pinned(directories.root),
            administration: Some(ComposedRootIdentity::from_pinned(
                directories.administration,
            )),
            common_administration: Some(ComposedRootIdentity::from_pinned(
                directories.common_administration,
            )),
        }
    }

    pub(super) const fn contains_directory(self, directory: ComposedRootIdentity) -> bool {
        self.root.is_the_same_directory_as(directory)
            || match self.common_administration {
                Some(administration) => administration.is_the_same_directory_as(directory),
                None => false,
            }
            || match self.administration {
                Some(administration) => administration.is_the_same_directory_as(directory),
                None => false,
            }
    }

    pub(super) const fn shares_a_directory_with(self, other: Self) -> bool {
        self.contains_directory(other.root)
            || match other.common_administration {
                Some(administration) => self.contains_directory(administration),
                None => false,
            }
            || match other.administration {
                Some(administration) => self.contains_directory(administration),
                None => false,
            }
    }
}

fn directory_identity(
    directory: &std::fs::File,
) -> Result<ComposedRootIdentity, DaemonToolsConstructionError> {
    let metadata = directory
        .metadata()
        .map_err(|_| DaemonToolsConstructionError::LocalGit)?;
    Ok(ComposedRootIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}
