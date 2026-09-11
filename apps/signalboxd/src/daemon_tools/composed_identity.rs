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
        if parent_identity.device != identity.device || crosses_mount(&directory, &parent)? {
            return Err(DaemonToolsConstructionError::LocalGit);
        }
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

#[cfg(target_os = "linux")]
fn crosses_mount(
    directory: &std::fs::File,
    parent: &std::fs::File,
) -> Result<bool, DaemonToolsConstructionError> {
    use rustix::fs::{AtFlags, StatxFlags, statx};
    let mount = |file: &std::fs::File| {
        let identity = statx(file, "", AtFlags::EMPTY_PATH, StatxFlags::MNT_ID)
            .map_err(|_| DaemonToolsConstructionError::LocalGit)?;
        if identity.stx_mask & StatxFlags::MNT_ID.bits() == 0 {
            return Err(DaemonToolsConstructionError::LocalGit);
        }
        Ok(identity.stx_mnt_id)
    };
    Ok(mount(directory)? != mount(parent)?)
}

#[cfg(not(target_os = "linux"))]
fn crosses_mount(
    _directory: &std::fs::File,
    _parent: &std::fs::File,
) -> Result<bool, DaemonToolsConstructionError> {
    Ok(false)
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

#[cfg(all(test, target_os = "linux"))]
mod mount_tests {
    use super::*;

    #[test]
    fn administration_ancestry_rejects_a_device_boundary() {
        let directory = std::fs::File::open("/proc").expect("proc mount");
        assert!(directory_ancestors(&directory).is_err());
    }

    #[test]
    #[ignore = "requires local Bubblewrap user and mount namespaces"]
    fn administration_ancestry_rejects_a_same_device_bind_mount() {
        const CHILD_ROOT: &str = "SIGNALBOX_BIND_ADMINISTRATION_FIXTURE";
        if let Some(root) = std::env::var_os(CHILD_ROOT) {
            assert!(matches!(
                ComposedWorkspaceIdentity::capture(Path::new(&root)),
                Err(DaemonToolsConstructionError::LocalGit)
            ));
            return;
        }
        let fixture = tempfile::tempdir().expect("bind fixture");
        let outer = fixture.path().join("outer");
        let inner = fixture.path().join("inner");
        let source = outer.join(".git/nested");
        let target = fixture.path().join("presented");
        git2::Repository::init(&outer).expect("outer repository");
        git2::Repository::init(&inner).expect("inner repository");
        std::fs::rename(inner.join(".git"), &source).expect("nested administration");
        std::fs::create_dir(&target).expect("mount target");
        std::fs::write(
            inner.join(".git"),
            format!("gitdir: {}\n", target.display()),
        )
        .expect("gitdir marker");
        assert_eq!(
            source.metadata().expect("source").dev(),
            target.metadata().expect("target").dev()
        );
        let output = std::process::Command::new("/usr/bin/bwrap")
            .args(["--unshare-user", "--unshare-pid", "--die-with-parent", "--ro-bind", "/", "/", "--bind"])
            .arg(&source)
            .arg(&target)
            .arg(std::env::current_exe().expect("test binary"))
            .args(["--exact", "daemon_tools::composed_identity::mount_tests::administration_ancestry_rejects_a_same_device_bind_mount", "--ignored", "--nocapture"])
            .env(CHILD_ROOT, &inner)
            .output()
            .expect("bind-mounted fixture child");
        assert!(
            output.status.success()
                && String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"),
            "bind-mounted administration must be rejected: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
