use std::{
    collections::BinaryHeap,
    error::Error,
    ffi::OsStr,
    fmt,
    fs::File,
    io::{self, Read},
    os::fd::OwnedFd,
    os::unix::ffi::OsStrExt,
    path::{Component, Path, PathBuf},
    sync::Arc,
};

use rustix::fs::{AtFlags, CWD, Dir, FileType, Mode, OFlags, fstat, openat, statat};

/// Maximum accepted UTF-8 byte length of one model-supplied workspace path.
pub const MAX_WORKSPACE_PATH_BYTES: usize = 4096;

/// Why a model-supplied path was rejected before filesystem access.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkspacePathRejection {
    /// The supplied path was absolute.
    Absolute,
    /// The supplied path contained a parent-directory component.
    ParentTraversal,
    /// The supplied path contained a NUL, exceeded the path byte bound, or had
    /// a component not representable by the relative-path contract.
    Invalid,
    /// A symbolic link occurred in the supplied path.
    Symlink,
}

impl fmt::Display for WorkspacePathRejection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Absolute => "absolute workspace path rejected",
            Self::ParentTraversal => "parent traversal in workspace path rejected",
            Self::Invalid => "invalid workspace path rejected",
            Self::Symlink => "symbolic link in workspace path rejected",
        })
    }
}

impl Error for WorkspacePathRejection {}

/// Construction failure for an injected workspace root.
#[derive(Debug)]
pub enum WorkspaceRootError {
    /// The injected root could not be opened without following a symlink.
    Io(io::Error),
    /// The opened injected root is not a directory.
    NotDirectory,
}

impl fmt::Display for WorkspaceRootError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Io(_) => "injected workspace root could not be opened",
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

/// Descriptor authority boundary injected into one workspace tool family.
#[derive(Clone)]
pub struct WorkspaceRoot {
    descriptor: Arc<OwnedFd>,
}

impl fmt::Debug for WorkspaceRoot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkspaceRoot")
            .finish_non_exhaustive()
    }
}

impl WorkspaceRoot {
    /// Opens and pins one injected directory without following a final symlink.
    pub fn try_new<FileSystem: WorkspaceFileSystem>(
        filesystem: &FileSystem,
        root: impl AsRef<Path>,
    ) -> Result<Self, WorkspaceRootError> {
        filesystem.open_root(root.as_ref())
    }

    fn open_local(root: &Path) -> Result<Self, WorkspaceRootError> {
        let descriptor = openat(
            CWD,
            root,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(io::Error::from)
        .map_err(WorkspaceRootError::Io)?;
        let status = fstat(&descriptor)
            .map_err(io::Error::from)
            .map_err(WorkspaceRootError::Io)?;
        if FileType::from_raw_mode(status.st_mode) != FileType::Directory {
            return Err(WorkspaceRootError::NotDirectory);
        }
        Ok(Self {
            descriptor: Arc::new(descriptor),
        })
    }

    /// Resolves an existing relative path through descriptor-relative,
    /// no-follow filesystem access.
    pub fn resolve_existing<FileSystem: WorkspaceFileSystem>(
        &self,
        filesystem: &FileSystem,
        supplied: &str,
    ) -> Result<PathBuf, WorkspaceResolveError> {
        validate_relative_path(supplied).map_err(WorkspaceResolveError::Rejected)?;
        let path = normalize_relative_path(Path::new(supplied));
        filesystem.entry_kind(self, &path)?;
        Ok(path)
    }

    pub(crate) fn relative_name(&self, path: &Path) -> Option<String> {
        let normalized = normalize_relative_path(path);
        Some(normalized.to_string_lossy().replace('\\', "/"))
    }
}

fn normalize_relative_path(path: &Path) -> PathBuf {
    let normalized = path
        .components()
        .filter_map(|component| match component {
            Component::Normal(name) => Some(name),
            Component::CurDir
            | Component::ParentDir
            | Component::RootDir
            | Component::Prefix(_) => None,
        })
        .collect::<PathBuf>();
    if normalized.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        normalized
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
    for component in path.components() {
        match component {
            Component::ParentDir => return Err(WorkspacePathRejection::ParentTraversal),
            Component::Normal(_) | Component::CurDir => {}
            Component::RootDir | Component::Prefix(_) => {
                return Err(WorkspacePathRejection::Invalid);
            }
        }
    }
    Ok(())
}

/// Failure while resolving or accessing one model-supplied path.
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
    /// Root-relative path, never an ambient absolute path.
    pub path: PathBuf,
    /// Closed entry kind from descriptor-relative no-follow metadata.
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
    /// Maximum file length observed from the same opened handle around reading.
    pub total_bytes: u64,
    /// Whether the bounded read observed bytes beyond the requested prefix.
    pub truncated: bool,
}

/// Injectable descriptor-relative filesystem operations needed by the tools.
pub trait WorkspaceFileSystem: Clone + Send + Sync + 'static {
    /// Opens and pins the injected root directory.
    fn open_root(&self, root: &Path) -> Result<WorkspaceRoot, WorkspaceRootError>;
    /// Classifies an existing root-relative path without following symlinks.
    fn entry_kind(
        &self,
        root: &WorkspaceRoot,
        path: &Path,
    ) -> Result<WorkspaceEntryKind, WorkspaceResolveError>;
    /// Returns a bounded lexical prefix of immediate root-relative entries.
    fn read_directory(
        &self,
        root: &WorkspaceRoot,
        path: &Path,
        max_entries: usize,
    ) -> Result<WorkspaceDirectoryRead, WorkspaceResolveError>;
    /// Reads a bounded prefix plus four lookahead bytes from a regular file.
    fn read_file_prefix(
        &self,
        root: &WorkspaceRoot,
        path: &Path,
        max_bytes: usize,
    ) -> Result<WorkspaceFileBytes, WorkspaceResolveError>;
}

/// Production adapter over descriptor-relative `rustix` operations.
#[derive(Clone, Copy, Debug, Default)]
pub struct LocalWorkspaceFileSystem;

impl WorkspaceFileSystem for LocalWorkspaceFileSystem {
    fn open_root(&self, root: &Path) -> Result<WorkspaceRoot, WorkspaceRootError> {
        WorkspaceRoot::open_local(root)
    }

    fn entry_kind(
        &self,
        root: &WorkspaceRoot,
        path: &Path,
    ) -> Result<WorkspaceEntryKind, WorkspaceResolveError> {
        let descriptor = open_relative(root, path, OFlags::RDONLY | OFlags::NONBLOCK)?;
        let status = fstat(&descriptor).map_err(resolve_io)?;
        Ok(entry_kind(FileType::from_raw_mode(status.st_mode)))
    }

    fn read_directory(
        &self,
        root: &WorkspaceRoot,
        path: &Path,
        max_entries: usize,
    ) -> Result<WorkspaceDirectoryRead, WorkspaceResolveError> {
        let descriptor = open_relative(root, path, OFlags::RDONLY | OFlags::DIRECTORY)?;
        let mut directory = Dir::new(descriptor).map_err(resolve_io)?;
        let mut retained = BinaryHeap::with_capacity(max_entries);
        let mut observed = 0_usize;
        while let Some(entry) = directory.read() {
            let entry = entry.map_err(resolve_io)?;
            let name_bytes = entry.file_name().to_bytes();
            if name_bytes == b"." || name_bytes == b".." {
                continue;
            }
            let name = OsStr::from_bytes(name_bytes);
            let status = statat(
                directory.fd().map_err(resolve_io)?,
                name,
                AtFlags::SYMLINK_NOFOLLOW,
            )
            .map_err(resolve_io)?;
            observed = observed.saturating_add(1);
            let candidate = WorkspaceDirectoryEntry {
                path: if path == Path::new(".") {
                    PathBuf::from(name)
                } else {
                    path.join(name)
                },
                kind: entry_kind(FileType::from_raw_mode(status.st_mode)),
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

    fn read_file_prefix(
        &self,
        root: &WorkspaceRoot,
        path: &Path,
        max_bytes: usize,
    ) -> Result<WorkspaceFileBytes, WorkspaceResolveError> {
        let descriptor = open_relative(root, path, OFlags::RDONLY | OFlags::NONBLOCK)?;
        let status = fstat(&descriptor).map_err(resolve_io)?;
        if FileType::from_raw_mode(status.st_mode) != FileType::RegularFile {
            return Err(WorkspaceResolveError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                "workspace path is not a regular file",
            )));
        }
        let initial_total_bytes = status.st_size.max(0) as u64;
        let lookahead = max_bytes.saturating_add(4);
        let mut bytes = Vec::with_capacity(lookahead);
        let mut file = File::from(descriptor);
        (&mut file)
            .take(lookahead as u64)
            .read_to_end(&mut bytes)
            .map_err(WorkspaceResolveError::Io)?;
        let final_total_bytes = file.metadata().map_err(WorkspaceResolveError::Io)?.len();
        let total_bytes = initial_total_bytes
            .max(final_total_bytes)
            .max(bytes.len() as u64);
        let truncated = bytes.len() > max_bytes || total_bytes > bytes.len() as u64;
        Ok(WorkspaceFileBytes {
            bytes,
            total_bytes,
            truncated,
        })
    }
}

fn open_relative(
    root: &WorkspaceRoot,
    path: &Path,
    final_flags: OFlags,
) -> Result<OwnedFd, WorkspaceResolveError> {
    validate_relative_path(path.to_str().ok_or(WorkspaceResolveError::Rejected(
        WorkspacePathRejection::Invalid,
    ))?)
    .map_err(WorkspaceResolveError::Rejected)?;
    let mut current = None;
    let mut components = path
        .components()
        .filter_map(|component| match component {
            Component::Normal(name) => Some(name),
            Component::CurDir => None,
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => None,
        })
        .peekable();
    if components.peek().is_none() {
        return openat(
            root.descriptor.as_ref(),
            ".",
            final_flags | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(resolve_io);
    }
    while let Some(name) = components.next() {
        let parent = current.as_ref().unwrap_or(root.descriptor.as_ref());
        let status = statat(parent, name, AtFlags::SYMLINK_NOFOLLOW).map_err(resolve_io)?;
        if FileType::from_raw_mode(status.st_mode) == FileType::Symlink {
            return Err(WorkspaceResolveError::Rejected(
                WorkspacePathRejection::Symlink,
            ));
        }
        let flags = if components.peek().is_some() {
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC
        } else {
            final_flags | OFlags::NOFOLLOW | OFlags::CLOEXEC
        };
        current = Some(openat(parent, name, flags, Mode::empty()).map_err(resolve_io)?);
    }
    current.ok_or_else(|| WorkspaceResolveError::Io(io::Error::other("empty workspace path")))
}

fn resolve_io(error: rustix::io::Errno) -> WorkspaceResolveError {
    if error == rustix::io::Errno::LOOP {
        WorkspaceResolveError::Rejected(WorkspacePathRejection::Symlink)
    } else {
        WorkspaceResolveError::Io(io::Error::from(error))
    }
}

fn entry_kind(file_type: FileType) -> WorkspaceEntryKind {
    match file_type {
        FileType::RegularFile => WorkspaceEntryKind::File,
        FileType::Directory => WorkspaceEntryKind::Directory,
        FileType::Symlink => WorkspaceEntryKind::Symlink,
        FileType::Fifo
        | FileType::Socket
        | FileType::CharacterDevice
        | FileType::BlockDevice
        | FileType::Unknown => WorkspaceEntryKind::Other,
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
    fn final_symlink_has_typed_rejection() {
        use std::{fs, os::unix::fs::symlink};

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
                WorkspacePathRejection::Symlink
            ))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn intermediate_symlink_has_typed_rejection() {
        use std::{fs, os::unix::fs::symlink};

        let workspace = tempfile::tempdir().expect("workspace fixture constructs");
        let outside = tempfile::tempdir().expect("outside fixture constructs");
        fs::write(outside.path().join("secret.txt"), "secret").expect("outside fixture writes");
        symlink(outside.path(), workspace.path().join("escape"))
            .expect("escape symlink fixture constructs");
        let filesystem = LocalWorkspaceFileSystem;
        let root =
            WorkspaceRoot::try_new(&filesystem, workspace.path()).expect("fixture root is valid");
        let result = root.resolve_existing(&filesystem, "escape/secret.txt");

        assert!(matches!(
            result,
            Err(WorkspaceResolveError::Rejected(
                WorkspacePathRejection::Symlink
            ))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_root_is_rejected_at_construction() {
        use std::{fs, os::unix::fs::symlink};

        let parent = tempfile::tempdir().expect("parent fixture constructs");
        let directory = parent.path().join("directory");
        let link = parent.path().join("workspace");
        fs::create_dir(&directory).expect("directory fixture constructs");
        symlink(&directory, &link).expect("root symlink fixture constructs");

        assert!(WorkspaceRoot::try_new(&LocalWorkspaceFileSystem, &link).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn pinned_root_descriptor_ignores_later_path_replacement() {
        use std::{fs, os::unix::fs::symlink};

        const ORIGINAL_CONTENT: &[u8] = b"original";

        let parent = tempfile::tempdir().expect("parent fixture constructs");
        let outside = tempfile::tempdir().expect("outside fixture constructs");
        let workspace_path = parent.path().join("workspace");
        let moved_path = parent.path().join("moved-workspace");
        fs::create_dir(&workspace_path).expect("workspace fixture constructs");
        fs::write(workspace_path.join("note.txt"), ORIGINAL_CONTENT)
            .expect("workspace fixture writes");
        fs::write(outside.path().join("note.txt"), "outside").expect("outside fixture writes");
        let filesystem = LocalWorkspaceFileSystem;
        let root =
            WorkspaceRoot::try_new(&filesystem, &workspace_path).expect("fixture root is valid");
        fs::rename(&workspace_path, &moved_path).expect("workspace fixture moves");
        symlink(outside.path(), &workspace_path).expect("replacement symlink fixture constructs");

        let read = filesystem
            .read_file_prefix(&root, Path::new("note.txt"), ORIGINAL_CONTENT.len())
            .expect("pinned workspace read succeeds");

        assert_eq!(read.bytes, ORIGINAL_CONTENT);
        assert_eq!(read.total_bytes, ORIGINAL_CONTENT.len() as u64);
    }

    #[cfg(unix)]
    #[test]
    fn fifo_read_is_rejected_without_blocking() {
        let workspace = tempfile::tempdir().expect("workspace fixture constructs");
        let fifo = workspace.path().join("pipe");
        rustix::fs::mkfifoat(
            rustix::fs::CWD,
            &fifo,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        )
        .expect("fifo fixture constructs");
        let filesystem = LocalWorkspaceFileSystem;
        let root =
            WorkspaceRoot::try_new(&filesystem, workspace.path()).expect("fixture root is valid");

        let result = filesystem.read_file_prefix(&root, Path::new("pipe"), 16);

        assert!(matches!(result, Err(WorkspaceResolveError::Io(_))));
    }

    #[cfg(unix)]
    #[test]
    fn final_component_symlink_swap_is_rejected_after_resolution() {
        use std::{fs, os::unix::fs::symlink};

        let workspace = tempfile::tempdir().expect("workspace fixture constructs");
        let outside = tempfile::tempdir().expect("outside fixture constructs");
        let workspace_file = workspace.path().join("note.txt");
        let outside_file = outside.path().join("secret.txt");
        fs::write(&workspace_file, "original").expect("workspace fixture writes");
        fs::write(&outside_file, "secret").expect("outside fixture writes");
        let filesystem = LocalWorkspaceFileSystem;
        let root =
            WorkspaceRoot::try_new(&filesystem, workspace.path()).expect("fixture root is valid");
        let resolved = root
            .resolve_existing(&filesystem, "note.txt")
            .expect("initial file resolves");
        fs::remove_file(&workspace_file).expect("workspace fixture removes");
        symlink(&outside_file, &workspace_file).expect("replacement symlink fixture constructs");

        let result = filesystem.read_file_prefix(&root, &resolved, 64);

        assert!(matches!(
            result,
            Err(WorkspaceResolveError::Rejected(
                WorkspacePathRejection::Symlink
            ))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn intermediate_component_symlink_swap_is_rejected_after_resolution() {
        use std::{fs, os::unix::fs::symlink};

        let workspace = tempfile::tempdir().expect("workspace fixture constructs");
        let outside = tempfile::tempdir().expect("outside fixture constructs");
        let directory = workspace.path().join("src");
        fs::create_dir(&directory).expect("directory fixture constructs");
        fs::write(directory.join("note.txt"), "original").expect("workspace fixture writes");
        fs::write(outside.path().join("note.txt"), "secret").expect("outside fixture writes");
        let filesystem = LocalWorkspaceFileSystem;
        let root =
            WorkspaceRoot::try_new(&filesystem, workspace.path()).expect("fixture root is valid");
        let resolved = root
            .resolve_existing(&filesystem, "src/note.txt")
            .expect("initial file resolves");
        fs::remove_dir_all(&directory).expect("directory fixture removes");
        symlink(outside.path(), &directory).expect("replacement symlink fixture constructs");

        let result = filesystem.read_file_prefix(&root, &resolved, 64);

        assert!(matches!(
            result,
            Err(WorkspaceResolveError::Rejected(
                WorkspacePathRejection::Symlink
            ))
        ));
    }
}
