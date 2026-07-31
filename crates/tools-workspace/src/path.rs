use std::{
    collections::BinaryHeap,
    error::Error,
    fmt,
    fs::{self, File},
    io::{self, Read},
    path::{Component, Path, PathBuf},
};

/// Maximum accepted UTF-8 byte length of one model-supplied workspace path.
pub const MAX_WORKSPACE_PATH_BYTES: usize = 4096;

/// Why a model-supplied path was rejected before filesystem access.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkspacePathRejection {
    /// The supplied path was absolute.
    Absolute,
    /// The supplied path contained a parent-directory component.
    ParentTraversal,
    /// The supplied path contained a NUL or exceeded the path byte bound.
    Invalid,
    /// Canonical resolution escaped the injected root through a symlink.
    OutsideRoot,
}

impl fmt::Display for WorkspacePathRejection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Absolute => "absolute workspace path rejected",
            Self::ParentTraversal => "parent traversal in workspace path rejected",
            Self::Invalid => "invalid workspace path rejected",
            Self::OutsideRoot => "workspace path resolves outside the injected root",
        })
    }
}

impl Error for WorkspacePathRejection {}

/// Construction failure for an injected workspace root.
#[derive(Debug)]
pub enum WorkspaceRootError {
    /// The injected root could not be canonicalized.
    Io(io::Error),
    /// The canonical injected root is not a directory.
    NotDirectory,
}

impl fmt::Display for WorkspaceRootError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Io(_) => "injected workspace root could not be canonicalized",
            Self::NotDirectory => "injected workspace root is not a directory",
        })
    }
}

impl Error for WorkspaceRootError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::NotDirectory => None,
        }
    }
}

/// Canonical authority boundary injected into one workspace tool family.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceRoot {
    canonical: PathBuf,
}

impl WorkspaceRoot {
    /// Canonicalizes and checks one injected directory through the injected
    /// filesystem.
    pub fn try_new<FileSystem: WorkspaceFileSystem>(
        filesystem: &FileSystem,
        root: impl AsRef<Path>,
    ) -> Result<Self, WorkspaceRootError> {
        let canonical = filesystem
            .canonicalize(root.as_ref())
            .map_err(WorkspaceRootError::Io)?;
        if !filesystem
            .metadata(&canonical)
            .map_err(WorkspaceRootError::Io)?
            .is_dir()
        {
            return Err(WorkspaceRootError::NotDirectory);
        }
        Ok(Self { canonical })
    }

    /// Resolves an existing relative path and rejects canonical escape.
    pub fn resolve_existing<FileSystem: WorkspaceFileSystem>(
        &self,
        filesystem: &FileSystem,
        supplied: &str,
    ) -> Result<PathBuf, WorkspaceResolveError> {
        validate_relative_path(supplied).map_err(WorkspaceResolveError::Rejected)?;
        self.ensure_existing(filesystem, &self.canonical.join(supplied))
    }

    pub(crate) fn ensure_existing<FileSystem: WorkspaceFileSystem>(
        &self,
        filesystem: &FileSystem,
        path: &Path,
    ) -> Result<PathBuf, WorkspaceResolveError> {
        let resolved = filesystem
            .canonicalize(path)
            .map_err(WorkspaceResolveError::Io)?;
        if !resolved.starts_with(&self.canonical) {
            return Err(WorkspaceResolveError::Rejected(
                WorkspacePathRejection::OutsideRoot,
            ));
        }
        Ok(resolved)
    }

    pub(crate) fn relative_name(&self, path: &Path) -> Option<String> {
        path.strip_prefix(&self.canonical).ok().map(|relative| {
            if relative.as_os_str().is_empty() {
                String::from(".")
            } else {
                relative.to_string_lossy().replace('\\', "/")
            }
        })
    }
}

pub(crate) fn validate_relative_path(supplied: &str) -> Result<(), WorkspacePathRejection> {
    if supplied.is_empty() || supplied.len() > MAX_WORKSPACE_PATH_BYTES || supplied.contains('\0') {
        return Err(WorkspacePathRejection::Invalid);
    }
    let path = Path::new(supplied);
    if path.is_absolute() {
        return Err(WorkspacePathRejection::Absolute);
    }
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(WorkspacePathRejection::ParentTraversal);
    }
    Ok(())
}

/// Failure while resolving one model-supplied path.
#[derive(Debug)]
pub enum WorkspaceResolveError {
    /// Typed evidence that the authority boundary rejected the path.
    Rejected(WorkspacePathRejection),
    /// The admitted path could not be resolved.
    Io(io::Error),
}

impl WorkspaceResolveError {
    /// Returns typed rejection evidence when the authority boundary rejected
    /// the path rather than encountering an I/O failure.
    pub const fn rejection(&self) -> Option<WorkspacePathRejection> {
        match self {
            Self::Rejected(reason) => Some(*reason),
            Self::Io(_) => None,
        }
    }
}

impl fmt::Display for WorkspaceResolveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rejected(reason) => reason.fmt(formatter),
            Self::Io(_) => formatter.write_str("workspace path could not be resolved"),
        }
    }
}

impl Error for WorkspaceResolveError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Rejected(reason) => Some(reason),
            Self::Io(error) => Some(error),
        }
    }
}

/// One filesystem entry returned by the injected adapter.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct WorkspaceDirectoryEntry {
    /// Absolute path used only inside the authority boundary.
    pub path: PathBuf,
    /// Closed entry kind from symlink metadata.
    pub kind: WorkspaceEntryKind,
}

/// Closed filesystem-entry kind exposed in tool results.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceEntryKind {
    /// A regular file.
    File,
    /// A directory.
    Directory,
    /// A symbolic link, which traversal never follows.
    Symlink,
    /// Another filesystem entry kind.
    Other,
}

/// One bounded directory read.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceDirectoryRead {
    /// Lexically smallest retained entries.
    pub entries: Vec<WorkspaceDirectoryEntry>,
    /// Whether additional entries were observed but omitted.
    pub truncated: bool,
}

/// One bounded file prefix and the size observed from the opened handle.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceFileBytes {
    /// Retained prefix, including up to four lookahead bytes for UTF-8 boundary
    /// detection.
    pub bytes: Vec<u8>,
    /// File length observed from the opened handle.
    pub total_bytes: u64,
}

/// Injectable filesystem operations needed by the workspace tools.
pub trait WorkspaceFileSystem: Clone + Send + Sync + 'static {
    /// Canonicalizes one path.
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf>;
    /// Reads metadata while following a final symlink.
    fn metadata(&self, path: &Path) -> io::Result<fs::Metadata>;
    /// Reads metadata without following a final symlink.
    fn symlink_metadata(&self, path: &Path) -> io::Result<fs::Metadata>;
    /// Returns a bounded lexical prefix of one directory's immediate entries.
    fn read_directory(&self, path: &Path, max_entries: usize)
    -> io::Result<WorkspaceDirectoryRead>;
    /// Reads a bounded prefix plus four lookahead bytes.
    fn read_file_prefix(&self, path: &Path, max_bytes: usize) -> io::Result<WorkspaceFileBytes>;
}

/// Production adapter over `std::fs`; it owns no ambient root.
#[derive(Clone, Copy, Debug, Default)]
pub struct LocalWorkspaceFileSystem;

impl WorkspaceFileSystem for LocalWorkspaceFileSystem {
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        fs::canonicalize(path)
    }

    fn metadata(&self, path: &Path) -> io::Result<fs::Metadata> {
        fs::metadata(path)
    }

    fn symlink_metadata(&self, path: &Path) -> io::Result<fs::Metadata> {
        fs::symlink_metadata(path)
    }

    fn read_directory(
        &self,
        path: &Path,
        max_entries: usize,
    ) -> io::Result<WorkspaceDirectoryRead> {
        let mut retained = BinaryHeap::with_capacity(max_entries);
        let mut observed = 0_usize;
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            let kind = if file_type.is_file() {
                WorkspaceEntryKind::File
            } else if file_type.is_dir() {
                WorkspaceEntryKind::Directory
            } else if file_type.is_symlink() {
                WorkspaceEntryKind::Symlink
            } else {
                WorkspaceEntryKind::Other
            };
            observed = observed.saturating_add(1);
            let candidate = WorkspaceDirectoryEntry {
                path: entry.path(),
                kind,
            };
            if retained.len() < max_entries {
                retained.push(candidate);
            } else if retained
                .peek()
                .is_some_and(|greatest| candidate < *greatest)
            {
                retained.pop();
                retained.push(candidate);
            }
        }
        Ok(WorkspaceDirectoryRead {
            entries: retained.into_sorted_vec(),
            truncated: observed > max_entries,
        })
    }

    fn read_file_prefix(&self, path: &Path, max_bytes: usize) -> io::Result<WorkspaceFileBytes> {
        let file = File::open(path)?;
        let total_bytes = file.metadata()?.len();
        let lookahead = max_bytes.saturating_add(4);
        let mut bytes = Vec::with_capacity(lookahead);
        file.take(lookahead as u64).read_to_end(&mut bytes)?;
        Ok(WorkspaceFileBytes { bytes, total_bytes })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolute_path_has_typed_rejection() {
        assert_eq!(
            validate_relative_path("/etc/passwd"),
            Err(WorkspacePathRejection::Absolute)
        );
    }

    #[test]
    fn parent_traversal_has_typed_rejection() {
        assert_eq!(
            validate_relative_path("src/../../secret"),
            Err(WorkspacePathRejection::ParentTraversal)
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_has_typed_rejection() {
        use std::os::unix::fs::symlink;

        let workspace = tempfile::tempdir().expect("workspace fixture constructs");
        let outside = tempfile::tempdir().expect("outside fixture constructs");
        let outside_file = outside.path().join("secret.txt");
        fs::write(&outside_file, "secret").expect("outside fixture writes");
        symlink(&outside_file, workspace.path().join("escape"))
            .expect("escape symlink fixture constructs");
        let filesystem = LocalWorkspaceFileSystem;
        let root =
            WorkspaceRoot::try_new(&filesystem, workspace.path()).expect("fixture root is valid");
        let result = root.resolve_existing(&filesystem, "escape");

        assert!(matches!(
            result,
            Err(WorkspaceResolveError::Rejected(
                WorkspacePathRejection::OutsideRoot
            ))
        ));
    }
}
