use super::{
    DaemonToolsConstructionError,
    session_workspace_roots::{ComposedRootIdentity, composed_root_identity},
};
use signalbox_tools_git::{PinnedRepositoryDirectories, open_repository_administration};
use std::{os::unix::fs::MetadataExt, path::Path};

/// Directories one composed workspace binds.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ComposedWorkspaceIdentity {
    /// Identity of the workspace root.
    pub root: ComposedRootIdentity,
    /// Identity of the worktree administration directory, when Git is present.
    pub administration: Option<ComposedRootIdentity>,
    /// Identity of shared references and objects, when Git is present.
    pub common_administration: Option<ComposedRootIdentity>,
    /// Ancestors of Git administration, captured through open directory descriptors.
    pub administration_ancestors: Vec<ComposedRootIdentity>,
}

impl ComposedWorkspaceIdentity {
    pub(super) fn capture(root: &Path) -> Result<Self, DaemonToolsConstructionError> {
        let root_identity = composed_root_identity(root)?;
        let (administration, common_administration, administration_ancestors) =
            match open_repository_administration(root)
                .map_err(|_| DaemonToolsConstructionError::LocalGit)?
            {
                Some(directories) => {
                    let mut ancestors = directory_ancestors(&directories.worktree)?;
                    if directory_identity(&directories.common)?
                        != directory_identity(&directories.worktree)?
                    {
                        ancestors.extend(directory_ancestors(&directories.common)?);
                    }
                    (
                        Some(directory_identity(&directories.worktree)?),
                        Some(directory_identity(&directories.common)?),
                        ancestors,
                    )
                }
                None => (None, None, Vec::new()),
            };
        if composed_root_identity(root)? != root_identity {
            return Err(DaemonToolsConstructionError::WorkspaceRootUnstable);
        }
        Ok(Self {
            root: root_identity,
            administration,
            common_administration,
            administration_ancestors,
        })
    }

    pub(super) fn from_pinned(
        directories: PinnedRepositoryDirectories,
        administration_ancestors: Vec<ComposedRootIdentity>,
    ) -> Self {
        Self {
            root: ComposedRootIdentity::from_pinned(directories.root),
            administration: Some(ComposedRootIdentity::from_pinned(
                directories.administration,
            )),
            common_administration: Some(ComposedRootIdentity::from_pinned(
                directories.common_administration,
            )),
            administration_ancestors,
        }
    }

    pub(super) const fn contains_directory(&self, directory: ComposedRootIdentity) -> bool {
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

    pub(super) fn shares_a_directory_with(&self, other: &Self) -> bool {
        self.contains_directory(other.root)
            || self
                .administration_ancestors
                .iter()
                .any(|ancestor| other.contains_directory(*ancestor))
            || other
                .administration_ancestors
                .iter()
                .any(|ancestor| self.contains_directory(*ancestor))
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

fn directory_ancestors(
    directory: &std::fs::File,
) -> Result<Vec<ComposedRootIdentity>, DaemonToolsConstructionError> {
    use rustix::fs::{Mode, OFlags, openat};
    #[cfg(target_os = "linux")]
    let access = OFlags::PATH;
    #[cfg(target_os = "macos")]
    let access = OFlags::from_bits_retain(libc::O_SEARCH as u32);
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let access = OFlags::RDONLY;
    let mut directory = directory
        .try_clone()
        .map_err(|_| DaemonToolsConstructionError::LocalGit)?;
    let mut ancestors = Vec::new();
    loop {
        let identity = directory_identity(&directory)?;
        let parent = std::fs::File::from(
            openat(
                &directory,
                "..",
                access | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|_| DaemonToolsConstructionError::LocalGit)?,
        );
        let parent_identity = directory_identity(&parent)?;
        if parent_identity == identity {
            return Ok(ancestors);
        }
        if ancestors.contains(&parent_identity) {
            return Err(DaemonToolsConstructionError::WorkspaceRootUnstable);
        }
        ancestors.push(parent_identity);
        directory = parent;
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
