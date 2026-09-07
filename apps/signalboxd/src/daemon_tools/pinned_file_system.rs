use signalbox_tools_workspace::{
    LocalWorkspaceFileSystem, WorkspaceDirectoryRead, WorkspaceEntryKind, WorkspaceFileBytes,
    WorkspaceFileMutation, WorkspaceFileSystem, WorkspaceMutationCommitError,
    WorkspaceMutationFileSystem, WorkspaceMutationPath, WorkspaceMutationSnapshot,
    WorkspaceMutationSnapshotError, WorkspaceResolveError, WorkspaceRoot, WorkspaceRootError,
};
use std::path::Path;

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
